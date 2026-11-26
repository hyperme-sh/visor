//! CLI and editor transport for one assigned terminal task.
use base64::{engine::general_purpose::STANDARD, Engine};
use hypermesh_visor::workspace_runtime::{wsl_distro, wsl_linux_path};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::time::Duration;
use uuid::Uuid;

mod input;

const ENDPOINT: &str = "/terminal-task";
const USAGE: &str = "Usage: hypermesh-companion --terminal-task WORKSPACE [--host DEVICE_ID] context | run REVISION | watch ID [CURSOR] | interact ID [CURSOR] [--take-over] | cancel ID | preview-status ID PORT | preview-open ID PORT | preview-close ID PORT PREVIEW_ID | request";

struct Connection<'a> {
    workspace: &'a str,
    device: Option<Uuid>,
}

impl Connection<'_> {
    fn exchange(&self, operation: Value) -> Result<Value, String> {
        let workspace = self.workspace;
        let distro = wsl_distro()?;
        let context = operation["type"] == "context";
        let mut body = json!({"workspace":workspace,"operation":operation});
        if let Some(distro) = distro {
            body["wsl_distro"] = json!(distro);
        }
        if let Some(device) = self.device {
            body["expected_device_id"] = json!(device);
        }
        let reply =
            crate::visor::trust_request(ENDPOINT, Some(body)).map_err(|error| match distro {
                Some(distro) => {
                    format!("Visor workspace {workspace} in WSL {distro} unavailable. {error}")
                }
                None => error,
            })?;
        if context
            && (distro.is_some() || self.device.is_some())
            && reply["workspace"].as_str() != Some(workspace)
        {
            return Err("Visor reported a different workspace".into());
        }
        if context && distro.is_some() && reply["wsl_distro"].as_str() != distro {
            return Err("Visor reported a different WSL distribution".into());
        }
        if context
            && self
                .device
                .is_some_and(|device| reply["device_id"] != json!(device))
        {
            return Err("Visor reported a different enrolled device".into());
        }
        Ok(reply)
    }
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
    connection: &Connection<'_>,
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
        let page = connection.exchange(request)?;
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
    let [workspace, tail @ ..] = args.as_slice() else {
        return Err(USAGE.into());
    };
    let (device, tail) = match tail {
        [flag, id, rest @ ..] if flag == "--host" => {
            let id = Uuid::parse_str(id).map_err(|_| "Invalid enrolled device ID")?;
            if id.is_nil() {
                return Err("Invalid enrolled device ID".into());
            }
            (Some(id), rest)
        }
        tail => (None, tail),
    };
    let [command, rest @ ..] = tail else {
        return Err(USAGE.into());
    };
    if wsl_distro()?.is_some() && !wsl_linux_path(workspace) {
        return Err("Use the absolute Linux workspace path inside the WSL distribution".into());
    }
    let workspace = std::fs::canonicalize(workspace).map_err(|_| "Local workspace unavailable")?;
    let workspace = workspace.to_str().ok_or("Invalid workspace path")?;
    let connection = Connection { workspace, device };
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
            println!("{}", connection.exchange(operation)?);
        }
        ("context", []) => println!("{}", connection.exchange(json!({"type":"context"}))?),
        ("run", [revision]) => {
            let scope = connection.exchange(json!({"type":"context"}))?;
            if scope["task"]["revision"].as_str() != Some(revision) {
                return Err("Review the current terminal task with context before starting".into());
            }
            let id = Uuid::new_v4();
            eprintln!("Task {id}");
            let mut request = action(&scope, "start", id);
            request["revision"] = json!(revision);
            connection.exchange(request)?;
            return watch(&connection, &scope, id, 0, None);
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
            let scope = connection.exchange(json!({"type":"context"}))?;
            return input::attach(&connection, &scope, id, cursor, take_over);
        }
        ("preview-status" | "preview-open" | "preview-close", [id, port, tail @ ..])
            if tail.len() == usize::from(command == "preview-close") =>
        {
            let id = Uuid::parse_str(id).map_err(|_| "Invalid task ID")?;
            let port = port.parse::<NonZeroU16>().map_err(|_| "Invalid preview port")?;
            let scope = connection.exchange(json!({"type":"context"}))?;
            let kind = match command.as_str() {
                "preview-open" => "preview_open",
                "preview-close" => "preview_close",
                _ => "preview_context",
            };
            let mut request = action(&scope, kind, id);
            request["port"] = json!(port);
            if let [preview] = tail {
                request["preview_id"] = json!(Uuid::parse_str(preview).map_err(|_| "Invalid preview ID")?);
            }
            println!("{}", connection.exchange(request)?);
        }
        ("watch" | "cancel", [id, ..])
            if rest.len() == 1 || command == "watch" && rest.len() == 2 =>
        {
            let id = Uuid::parse_str(id).map_err(|_| "Invalid task ID")?;
            let scope = connection.exchange(json!({"type":"context"}))?;
            if command == "watch" {
                let cursor = rest
                    .get(1)
                    .map(|v| v.parse().map_err(|_| "Invalid cursor"))
                    .transpose()?
                    .unwrap_or(0);
                return watch(&connection, &scope, id, cursor, None);
            }
            println!("{}", connection.exchange(action(&scope, "cancel", id))?);
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
