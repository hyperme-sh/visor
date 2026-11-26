//! Standalone SDK for the public Visor stdio API. All Visor authority stays with the host.
use serde_json::{json, Value};
use std::io::{self, BufRead, Read, Write};
use std::time::Duration;

const MAX_FRAME: usize = 96 * 1024;

/// A single admitted invocation. V0 omits the advisory remaining deadline.
pub struct Call {
    pub name: String,
    pub input: Value,
    pub remaining: Option<Duration>,
}

/// Serve one invocation and exit. The host can terminate the process at any time.
pub fn serve(
    plugin_id: &str,
    versions: &[u32],
    handler: impl FnOnce(Call) -> Result<Value, ToolError>,
) -> io::Result<()> {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let init = read(&mut stdin)?;
    let id = request_id(&init, "initialize")?;
    let proposed = init["params"]["api_versions"]
        .as_array()
        .ok_or_else(invalid)?;
    let version = proposed.iter().find_map(|v| match v.as_u64() {
        Some(0) if versions.contains(&0) => Some(0),
        Some(1) if versions.contains(&1) => Some(1),
        _ => None,
    });
    let Some(version) = version.filter(|_| init["params"]["plugin_id"] == plugin_id) else {
        return write(
            &mut stdout,
            failure(id, -32001, "Incompatible plugin identity or API"),
        );
    };
    write(
        &mut stdout,
        json!({
            "jsonrpc":"2.0","id":id,"result":{"plugin_id":plugin_id,"api_version":version}
        }),
    )?;
    let call = read(&mut stdin)?;
    let id = request_id(&call, "tools/call")?;
    let name = call["params"]["name"].as_str().ok_or_else(invalid)?;
    let input = &call["params"]["input"];
    let remaining = if version == 1 {
        let millis = call["params"]["timeout_ms"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(invalid)?;
        Some(Duration::from_millis(millis))
    } else {
        None
    };
    if !input.is_object() {
        return write(&mut stdout, failure(id, -32602, "Invalid tool input"));
    }
    let result = handler(Call {
        name: name.into(),
        input: input.clone(),
        remaining,
    });
    let response = match result {
        Ok(value) if value.is_object() => json!({"jsonrpc":"2.0","id":id,"result":value}),
        Ok(_) => failure(id, -32603, "Invalid tool output"),
        Err(ToolError::UnknownTool) => failure(id, -32601, "Unknown tool"),
        Err(ToolError::InvalidInput) => failure(id, -32602, "Invalid tool input"),
        Err(ToolError::Failed) => failure(id, -32603, "Tool failed"),
    };
    write(&mut stdout, response)
}

#[derive(Debug)]
pub enum ToolError {
    UnknownTool,
    InvalidInput,
    Failed,
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Invalid Visor plugin frame")
}

fn read(input: &mut impl BufRead) -> io::Result<Value> {
    let mut line = Vec::new();
    input
        .take(MAX_FRAME as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if line.len() > MAX_FRAME || line.last() != Some(&b'\n') {
        return Err(invalid());
    }
    serde_json::from_slice(&line).map_err(|_| invalid())
}

fn write(output: &mut impl Write, frame: Value) -> io::Result<()> {
    let raw = serde_json::to_vec(&frame).map_err(|_| invalid())?;
    if raw.len() >= MAX_FRAME {
        return Err(invalid());
    }
    output.write_all(&raw)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn request_id<'a>(value: &'a Value, method: &str) -> io::Result<&'a Value> {
    if value["jsonrpc"] != "2.0"
        || value["method"] != method
        || !value["params"].is_object()
        || value.get("result").is_some()
        || value.get("error").is_some()
    {
        return Err(invalid());
    }
    value.get("id").filter(|id| id.is_u64()).ok_or_else(invalid)
}

fn failure(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
