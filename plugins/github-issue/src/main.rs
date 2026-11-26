use visor_plugin_sdk::{serve, ToolError};

mod code;
mod sandbox;

fn main() -> std::io::Result<()> {
    use std::io::Write;

    let mut args = std::env::args_os().skip(1);
    let mut mode = args.next();
    let sandboxed = mode.as_deref().is_some_and(|arg| arg == "--sandbox-v1");
    if sandboxed {
        sandbox::install()?;
        sandbox::verify()?;
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(b"{\"sandbox\":1}\n")?;
        stdout.flush()?;
        mode = args.next();
    }
    match mode.as_deref() {
        Some(arg) if arg == "--code-mode-v1" => return code::run(),
        Some(arg) if arg == "--third-party-notices" => {
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
