use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::json;
use visor_plugin_sdk::{serve, ToolError};

fn main() -> std::io::Result<()> {
    serve("org.example.byte-count", &[1, 0], |call| {
        if call.name != "count-bytes" {
            return Err(ToolError::UnknownTool);
        }
        let input = call.input["content_base64"]
            .as_str()
            .ok_or(ToolError::InvalidInput)?;
        let bytes = STANDARD
            .decode(input)
            .map_err(|_| ToolError::InvalidInput)?;
        Ok(json!({"byte_count": bytes.len()}))
    })
}
