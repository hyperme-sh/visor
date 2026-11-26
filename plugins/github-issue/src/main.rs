use visor_plugin_sdk::{serve, ToolError};

mod code;

fn main() -> std::io::Result<()> {
    match std::env::args_os().nth(1).as_deref() {
        Some(arg) if arg == "--code-mode-v1" => return code::run(),
        Some(arg) if arg == "--third-party-notices" => {
            use std::io::Write;
            return std::io::stdout()
                .lock()
                .write_all(include_bytes!("../THIRD_PARTY_NOTICES"));
        }
        _ => {}
    }
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
