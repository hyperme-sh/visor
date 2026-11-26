//! CLI and editor transport for one assigned terminal task.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::time::Duration;
use uuid::Uuid;

const ENDPOINT: &str = "/terminal-task";
const USAGE: &str = "Usage: hypermesh-companion --terminal-task WORKSPACE context | run REVISION | watch ID [CURSOR] | cancel ID | request";

fn exchange(workspace: &str, operation: Value) -> Result<Value, String> {
    crate::visor::trust_request(
        ENDPOINT,
        Some(json!({"workspace":workspace,"operation":operation})),
    )
}

fn action(scope: &Value, kind: &str, id: Uuid) -> Value {
    json!({"type":kind,"boot":scope["boot"],"session":scope["session"],"binding":scope["binding"],"id":id})
}

fn watch(workspace: &str, scope: &Value, id: Uuid, mut cursor: u64) -> Result<bool, String> {
    let mut prior_status = String::new();
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
        let output = STANDARD
            .decode(
                page["output_base64"]
                    .as_str()
                    .ok_or("Invalid terminal output")?,
            )
            .map_err(|_| "Invalid terminal output")?;
        // Task output is data. In particular, do not replay escape sequences.
        let safe: String = String::from_utf8_lossy(&output)
            .chars()
            .filter(|c| matches!(c, '\n' | '\t') || !c.is_control())
            .collect();
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(safe.as_bytes())
            .and_then(|_| stdout.flush())
            .map_err(|_| "Could not write task output")?;
        drop(stdout);
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
            return watch(workspace, &scope, id, 0);
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
                return watch(workspace, &scope, id, cursor);
            }
            println!("{}", exchange(workspace, action(&scope, "cancel", id))?);
        }
        _ => return Err(USAGE.into()),
    }
    Ok(true)
}
