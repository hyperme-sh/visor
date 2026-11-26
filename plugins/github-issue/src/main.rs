use visor_plugin_sdk::{serve, ToolError};

fn main() -> std::io::Result<()> {
    serve("dev.visor.github-issue", &[1], |call| {
        if call.name != "read-issue" {
            return Err(ToolError::UnknownTool);
        }
        // Visor admits and shapes this issue before starting the plugin.
        let issue = call.input.get("issue").ok_or(ToolError::InvalidInput)?;
        if !issue.is_object() {
            return Err(ToolError::InvalidInput);
        }
        Ok(issue.clone())
    })
}
