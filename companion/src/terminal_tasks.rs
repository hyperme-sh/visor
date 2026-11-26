//! CLI and editor transport for one assigned terminal task.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::time::Duration;
use uuid::Uuid;

mod input;

const ENDPOINT: &str = "/terminal-task";
const USAGE: &str = "Usage: hypermesh-companion --terminal-task WORKSPACE context | run REVISION | watch ID [CURSOR] | interact ID [CURSOR] [--take-over] | cancel ID | request";

fn exchange(workspace: &str, operation: Value) -> Result<Value, String> {
    crate::visor::trust_request(
        ENDPOINT,
        Some(json!({"workspace":workspace,"operation":operation})),
    )
}

fn action(scope: &Value, kind: &str, id: Uuid) -> Value {
    json!({"type":kind,"boot":scope["boot"],"session":scope["session"],"binding":scope["binding"],"id":id})
}

#[derive(Default)]
struct OutputDecoder {
    pending: Vec<u8>,
}

impl OutputDecoder {
    fn render(&mut self, bytes: &[u8], finished: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let length = self.pending.len();
        let mut end = length;
        if !finished {
            // An incomplete UTF-8 prefix occupies at most three trailing bytes.
            for suffix in 1..=length.min(3) {
                let start = length - suffix;
                if std::str::from_utf8(&self.pending[start..])
                    .is_err_and(|error| error.valid_up_to() == 0 && error.error_len().is_none())
                {
                    end = start;
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&self.pending[..end])
            .chars()
            .filter(|c| matches!(c, '\n' | '\t') || !c.is_control())
            .collect();
        self.pending.drain(..end);
        text
    }
}

fn watch(
    workspace: &str,
    scope: &Value,
    id: Uuid,
    mut cursor: u64,
    mut input: Option<&mut dyn input::Attachment>,
) -> Result<bool, String> {
    let mut prior_status = String::new();
    let mut prior_owner: Option<Value> = None;
    let mut decoder = OutputDecoder::default();
    loop {
        let mut request = action(scope, "read", id);
        request["cursor"] = json!(cursor);
        let page = exchange(workspace, request)?;
        cursor = page["cursor"]
            .as_u64()
            .ok_or("Invalid terminal task cursor")?;
        let status = page["status"]
            .as_str()
            .ok_or("Invalid terminal task state")?;
        let access = page["access"]
            .as_str()
            .ok_or("Invalid terminal task access")?;
        if status != prior_status {
            eprintln!(
                "Task {id} · {status} · access {access} · exit {}",
                page["exit_code"]
            );
            prior_status = status.to_owned();
        }
        let owner = &page["terminal"]["owner"];
        let changed = prior_owner
            .as_ref()
            .is_none_or(|previous| previous != &owner["id"]);
        if page["terminal"].is_object() && changed {
            eprintln!(
                "Input owner: {} · last ack {}",
                owner["client"].as_str().unwrap_or("none"),
                owner["ack"]["sequence"]
            );
            prior_owner = Some(owner["id"].clone());
        }
        let output = STANDARD
            .decode(
                page["output_base64"]
                    .as_str()
                    .ok_or("Invalid terminal output")?,
            )
            .map_err(|_| "Invalid terminal output")?;
        let finished = page["more"] == false && status != "running";
        let safe = decoder.render(&output, finished);
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(safe.as_bytes())
            .and_then(|_| stdout.flush())
            .map_err(|_| "Could not write task output")?;
        drop(stdout);
        if status == "running" {
            if let Some(attachment) = input.as_deref_mut() {
                if !attachment.advance(&page)? {
                    eprintln!("Output cursor {cursor}");
                    return Ok(true);
                }
            }
        }
        if page["more"] != true {
            if status != "running" {
                if page["truncated"] == true {
                    eprintln!("Terminal task output reached its limit.");
                }
                return Ok(status == "completed");
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

pub(crate) fn run(args: Vec<String>) -> Result<bool, String> {
    let [workspace, command, rest @ ..] = args.as_slice() else {
        return Err(USAGE.into());
    };
    let workspace = std::fs::canonicalize(workspace).map_err(|_| "Local workspace unavailable")?;
    let workspace = workspace.to_str().ok_or("Invalid workspace path")?;
    match (command.as_str(), rest) {
        ("request", []) => {
            let mut input = Vec::new();
            std::io::stdin()
                .take(8193)
                .read_to_end(&mut input)
                .map_err(|_| "Cannot read terminal request")?;
            if input.len() > 8192 {
                return Err("Terminal request exceeds its limit".into());
            }
            let operation =
                serde_json::from_slice(&input).map_err(|_| "Invalid terminal request")?;
            println!("{}", exchange(workspace, operation)?);
        }
        ("context", []) => println!("{}", exchange(workspace, json!({"type":"context"}))?),
        ("run", [revision]) => {
            let scope = exchange(workspace, json!({"type":"context"}))?;
            if scope["task"]["revision"].as_str() != Some(revision) {
                return Err("Review the current terminal task with context before starting".into());
            }
            let id = Uuid::new_v4();
            eprintln!("Task {id}");
            let mut request = action(&scope, "start", id);
            request["revision"] = json!(revision);
            exchange(workspace, request)?;
            return watch(workspace, &scope, id, 0, None);
        }
        ("interact", [id, options @ ..]) => {
            let id = Uuid::parse_str(id).map_err(|_| "Invalid task ID")?;
            let (options, take_over) = match options.split_last() {
                Some((flag, rest)) if flag == "--take-over" => (rest, true),
                _ => (options, false),
            };
            let cursor = match options {
                [] => 0,
                [value] => value.parse().map_err(|_| "Invalid output cursor")?,
                _ => return Err(USAGE.into()),
            };
            let scope = exchange(workspace, json!({"type":"context"}))?;
            return input::attach(workspace, &scope, id, cursor, take_over);
        }
        ("watch" | "cancel", [id, ..])
            if rest.len() == 1 || command == "watch" && rest.len() == 2 =>
        {
            let id = Uuid::parse_str(id).map_err(|_| "Invalid task ID")?;
            let scope = exchange(workspace, json!({"type":"context"}))?;
            if command == "watch" {
                let cursor = rest
                    .get(1)
                    .map(|v| v.parse().map_err(|_| "Invalid cursor"))
                    .transpose()?
                    .unwrap_or(0);
                return watch(workspace, &scope, id, cursor, None);
            }
            println!("{}", exchange(workspace, action(&scope, "cancel", id))?);
        }
        _ => return Err(USAGE.into()),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_decoder_preserves_unicode_across_pages_and_filters_controls() {
        let mut decoder = OutputDecoder::default();
        assert_eq!(decoder.render(&[b'A', 0xff, 0xf0, 0x9f], false), "A�");
        assert_eq!(decoder.render(&[0x8c], false), "");
        assert_eq!(decoder.render(&[0x8d, 0x1b, b'\n'], false), "🌍\n");
        assert_eq!(decoder.render(&[0xe2], true), "�");
    }
}
