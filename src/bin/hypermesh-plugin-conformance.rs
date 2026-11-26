//! Probe a developer-supplied package with synthetic input in the Visor sandbox.
#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    use std::{io::Read, path::Path, process::ExitCode};
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [manifest, digest, tool, scratch] = args.as_slice() else {
        eprintln!("Usage: hypermesh-plugin-conformance ABS_MANIFEST SHA256 TOOL PRIVATE_SCRATCH < INPUT.json");
        return ExitCode::FAILURE;
    };
    let Some((digest, tool)) = digest.to_str().zip(tool.to_str()) else {
        eprintln!("Manifest SHA256 and tool must be UTF-8");
        return ExitCode::FAILURE;
    };
    let mut raw = Vec::new();
    if std::io::stdin()
        .take(64 * 1024 + 1)
        .read_to_end(&mut raw)
        .is_err()
        || raw.len() > 64 * 1024
    {
        eprintln!("Invalid or oversized fixture input");
        return ExitCode::FAILURE;
    }
    let Ok(input) = serde_json::from_slice(&raw) else {
        eprintln!("Invalid JSON fixture input");
        return ExitCode::FAILURE;
    };
    match hypermesh_visor::plugins::conformance(
        Path::new(manifest),
        digest,
        tool,
        Path::new(scratch),
        input,
    )
    .await
    {
        Ok(result) => {
            println!("{result}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{}: {error}", error.code());
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("Plugin conformance requires Linux");
    std::process::ExitCode::FAILURE
}
