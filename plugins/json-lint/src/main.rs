use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::collections::HashSet;
use visor_plugin_sdk::{serve, ToolError};

const MAX_DIAGNOSTICS: usize = 32;

fn main() -> std::io::Result<()> {
    serve("sh.hyperme.json-lint", &[1], |call| {
        if call.name != "lint-json" {
            return Err(ToolError::UnknownTool);
        }
        let input = call.input["content_base64"]
            .as_str()
            .ok_or(ToolError::InvalidInput)?;
        let bytes = STANDARD
            .decode(input)
            .map_err(|_| ToolError::InvalidInput)?;
        if bytes.len() > 32 * 1024 {
            return Err(ToolError::InvalidInput);
        }
        Ok(lint(&bytes))
    })
}

fn diagnostic(text: &str, offset: usize, rule: &str) -> Value {
    // VS Code uses one line break for CRLF and also recognizes lone CR.
    let before = &text[..offset];
    let before = before.strip_prefix('\u{feff}').unwrap_or(before);
    let normalized = before.replace("\r\n", "\n").replace('\r', "\n");
    let line = normalized.bytes().filter(|&byte| byte == b'\n').count() + 1;
    let column = normalized
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .encode_utf16()
        .count()
        + 1;
    json!({"line":line,"column":column,"rule":rule})
}

fn lint(bytes: &[u8]) -> Value {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => {
            let prefix = std::str::from_utf8(&bytes[..err.valid_up_to()]).expect("valid prefix");
            return json!({"diagnostics":[diagnostic(prefix, prefix.len(), "json-encoding")],"truncated":false});
        }
    };
    // Accept large numbers without narrowing them to a machine integer or float.
    if let Err(error) = serde_json::from_str::<serde::de::IgnoredAny>(text) {
        let mut start = 0;
        for line in text
            .split_inclusive('\n')
            .take(error.line().saturating_sub(1))
        {
            start += line.len();
        }
        let end = text[start..]
            .find('\n')
            .map_or(text.len(), |index| start + index);
        let column = if error.is_eof() {
            error.column()
        } else {
            error.column().saturating_sub(1)
        };
        let mut offset = (start + column).min(end);
        while !text.is_char_boundary(offset) {
            offset -= 1;
        }
        return json!({"diagnostics":[diagnostic(text, offset, "json-syntax")],"truncated":false});
    }

    // The input is already syntactically valid. An object key is a string followed by a colon.
    let mut objects = Vec::new();
    let mut findings = Vec::new();
    let mut truncated = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => objects.push(HashSet::new()),
            b'}' => {
                objects.pop();
            }
            b'"' => {
                let start = i;
                i += 1;
                while bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                // Validate Unicode escapes, then compare keys by their decoded spelling.
                let Ok(value) = serde_json::from_str::<String>(&text[start..=i]) else {
                    return json!({"diagnostics":[diagnostic(text, start, "json-syntax")],"truncated":false});
                };
                let next = bytes[i + 1..]
                    .iter()
                    .find(|byte| !byte.is_ascii_whitespace());
                if next == Some(&b':') && !objects.last_mut().expect("object key").insert(value) {
                    if findings.len() == MAX_DIAGNOSTICS {
                        truncated = true;
                    } else {
                        findings.push(diagnostic(text, start, "duplicate-key"));
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    json!({"diagnostics":findings,"truncated":truncated})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_keys_are_scoped_and_columns_use_utf16() {
        let text = "{\"😀\":0,\"\\u0061\":{\"a\":0},\"a\":1}";
        assert_eq!(
            lint(text.as_bytes()),
            json!({
                "diagnostics":[{"rule":"duplicate-key","line":1,"column":26}],"truncated":false
            })
        );
        assert_eq!(lint(br#"[{"a":1},{"a":2}]"#)["diagnostics"], json!([]));
    }

    #[test]
    fn syntax_encoding_eof_and_limits() {
        assert_eq!(
            lint(b"{\n \"x\": }")["diagnostics"][0],
            json!({"rule":"json-syntax","line":2,"column":7})
        );
        assert_eq!(
            lint(b"{\n")["diagnostics"][0],
            json!({"rule":"json-syntax","line":2,"column":1})
        );
        assert_eq!(
            lint(&[b' ', 0xff])["diagnostics"][0],
            json!({"rule":"json-encoding","line":1,"column":2})
        );
        assert_eq!(
            lint(br#""\uD800""#)["diagnostics"][0]["rule"],
            "json-syntax"
        );
        assert_eq!(
            lint(br#"{"$serde_json::private::Number": 1e9999}"#)["diagnostics"],
            json!([])
        );
        assert_eq!(lint(b"{\r \"x\": }")["diagnostics"][0]["line"], 2);
        let text = format!("{{{}\"a\":0}}", "\"a\":0,".repeat(35));
        let result = lint(text.as_bytes());
        assert_eq!(
            result["diagnostics"].as_array().unwrap().len(),
            MAX_DIAGNOSTICS
        );
        assert_eq!(result["truncated"], true);
    }
}
