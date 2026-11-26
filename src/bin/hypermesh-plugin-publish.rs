//! Sign an existing plugin package. Only the publication goes to stdout.
#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use sha2::{Digest, Sha256};
    use std::{
        io::{Read, Write},
        path::Path,
        process::ExitCode,
    };
    use zeroize::Zeroizing;

    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let (package, source, domain) = match args.as_slice() {
        [package, source] => (package, source.to_str(), None),
        [package, source, domain] if domain.to_str().is_some() => {
            (package, source.to_str(), domain.to_str())
        }
        _ => {
            eprintln!("Usage: hypermesh-plugin-publish PACKAGE HTTPS_SOURCE [PUBLISHER_DOMAIN] < KEY.der > PUBLICATION");
            return ExitCode::FAILURE;
        }
    };
    let Some(source) = source else {
        eprintln!("The publication source must be UTF-8");
        return ExitCode::FAILURE;
    };

    let mut key = Zeroizing::new(Vec::new());
    if std::io::stdin().take(4097).read_to_end(&mut key).is_err() || key.len() > 4096 {
        eprintln!("Invalid Ed25519 private key input");
        return ExitCode::FAILURE;
    }
    let (artifact, public_key) =
        match hypermesh_visor::plugins::build_publication(Path::new(package), &key, source, domain)
        {
            Ok(output) => output,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        };
    let mut output = std::io::stdout().lock();
    if output
        .write_all(&artifact)
        .and_then(|_| output.flush())
        .is_err()
    {
        eprintln!("Could not write the plugin publication");
        return ExitCode::FAILURE;
    }
    eprintln!(
        "{}",
        serde_json::json!({
            "publication_sha256": format!("{:x}", Sha256::digest(&artifact)),
            "publisher_key": public_key,
        })
    );
    ExitCode::SUCCESS
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("The plugin publisher requires Linux");
    std::process::ExitCode::FAILURE
}
