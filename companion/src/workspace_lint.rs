use serde_json::{json, Value};
use std::{path::Path, process::ExitCode};

fn request(args: &[String]) -> Result<Value, String> {
    let [workspace, file] = args else {
        return Err(
            "Usage: hypermesh-companion --workspace-lint WORKSPACE RELATIVE_JSON_FILE".into(),
        );
    };
    let workspace = Path::new(workspace)
        .canonicalize()
        .map_err(|_| "Open an assigned workspace")?;
    let workspace = workspace.to_str().ok_or("Workspace path must be UTF-8")?;
    crate::visor::plugin_request(
        "POST",
        "/workspace-lint",
        &json!({
            "workspace":workspace,"file":file
        }),
    )
}

fn cli_response(response: Result<Value, String>) -> (u8, Value) {
    match response {
        Ok(value) if value["error"]["message"].is_string() => (2, value),
        Ok(value) if value["result"]["diagnostics"].is_array() => (
            u8::from(
                !value["result"]["diagnostics"]
                    .as_array()
                    .unwrap()
                    .is_empty(),
            ),
            value,
        ),
        Ok(_) => (
            2,
            json!({"error":{"message":"Invalid workspace linter response"}}),
        ),
        Err(message) => (2, json!({"error":{"message":message}})),
    }
}

pub fn run(args: Vec<String>) -> ExitCode {
    let (code, body) = cli_response(request(&args));
    println!("{body}");
    ExitCode::from(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_findings_and_tool_failures_have_distinct_exit_status() {
        let clean = json!({"result":{"diagnostics":[]}});
        let findings =
            json!({"result":{"diagnostics":[{"line":2,"column":5,"rule":"duplicate-key"}]}});
        let error = json!({"error":{"code":"read_refused","message":"Workspace read refused"}});
        assert_eq!(cli_response(Ok(clean.clone())), (0, clean));
        assert_eq!(cli_response(Ok(findings.clone())), (1, findings));
        assert_eq!(cli_response(Ok(error.clone())), (2, error));
        assert_eq!(cli_response(Err("Enable plugin".into())).0, 2);
    }
}
