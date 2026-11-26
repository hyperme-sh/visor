//! Readiness through the existing same-user control socket.
use http_body_util::{BodyExt, Empty, Limited};
use hyper::{body::Bytes, client::conn::http1, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::{process::ExitCode, time::Duration};

const ERROR: &str = "Headless Visor is unavailable";

pub(crate) fn run() -> ExitCode {
    let status = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ERROR)
        .and_then(|runtime| runtime.block_on(status()));
    match status {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn status() -> Result<Value, &'static str> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let runtime = hypermesh_visor::runtime_dir().map_err(|_| ERROR)?;
        let endpoint = hypermesh_local_transport::Endpoint::unix(
            hypermesh_visor::control_socket_file(&runtime),
        )
        .map_err(|_| ERROR)?;
        let stream = hypermesh_local_transport::connect(&endpoint)
            .await
            .map_err(|_| ERROR)?;
        let (mut client, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|_| ERROR)?;
        tokio::spawn(connection);
        let request = Request::get("/capabilities")
            .header("Host", "localhost")
            .body(Empty::<Bytes>::new())
            .map_err(|_| ERROR)?;
        let response = client.send_request(request).await.map_err(|_| ERROR)?;
        if response.status() != StatusCode::OK {
            return Err(ERROR);
        }
        let response = Limited::new(response.into_body(), 32 * 1024)
            .collect()
            .await
            .map_err(|_| ERROR)?;
        let value: Value = serde_json::from_slice(&response.to_bytes()).map_err(|_| ERROR)?;
        if value["build_mode"] != "headless" {
            return Err(ERROR);
        }
        Ok(serde_json::json!({
            "ready": true,
            "build_mode": "headless",
            "tools": installed_tools(),
        }))
    })
    .await
    .map_err(|_| ERROR)?
}

fn installed_tools() -> serde_json::Map<String, Value> {
    let Ok(inventory) = std::fs::read_to_string("/usr/share/visor-worker/packages.tsv") else {
        return serde_json::Map::new();
    };
    inventory
        .lines()
        .filter_map(|line| {
            let mut columns = line.split('\t');
            let name = columns.next()?;
            let version = columns.next()?;
            if !matches!(name, "bash" | "git" | "shellcheck")
                || columns.nth(3) != Some("installed")
                || !std::path::Path::new("/usr/bin").join(name).is_file()
            {
                return None;
            }
            Some((name.to_owned(), Value::String(version.to_owned())))
        })
        .collect()
}
