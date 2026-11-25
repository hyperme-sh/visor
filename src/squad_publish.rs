use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;
use uuid::Uuid;

const MAX_DRAFT_BYTES: u64 = 65_536;

fn request_body(
    args: impl Iterator<Item = String>,
    input: impl Read,
) -> Result<Vec<u8>, &'static str> {
    let args: Vec<_> = args.collect();
    let [id] = args.as_slice() else {
        return Err("usage: hypermesh-visor squad-publish PLANE_TASK_ID");
    };
    let task = Uuid::parse_str(id)
        .ok()
        .filter(|task| !task.is_nil() && task.to_string() == *id)
        .ok_or("invalid Plane task ID")?;
    let mut draft = Vec::new();
    input
        .take(MAX_DRAFT_BYTES + 1)
        .read_to_end(&mut draft)
        .map_err(|_| "could not read unsigned Squad artifact")?;
    if draft.len() as u64 > MAX_DRAFT_BYTES {
        return Err("unsigned Squad artifact exceeds frame limit");
    }
    let artifact =
        serde_json::from_slice::<Value>(&draft).map_err(|_| "invalid Squad artifact JSON")?;
    serde_json::to_vec(&json!({ "planeTaskId": task, "artifact": artifact }))
        .map_err(|_| "invalid Squad artifact")
}

async fn publish(body: Vec<u8>) -> Result<Bytes, &'static str> {
    let socket = hypermesh_visor::runtime_dir()
        .map_err(|_| "could not resolve Visor runtime")?
        .join("companion.sock");
    let endpoint = hypermesh_local_transport::Endpoint::unix(socket)
        .map_err(|_| "invalid Visor companion socket")?;
    let stream = hypermesh_local_transport::connect(&endpoint)
        .await
        .map_err(|_| "could not reach private Visor companion socket")?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| "Visor companion handshake failed")?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let request = hyper::Request::post("/squad/publish")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("connection", "close")
        .body(Full::new(Bytes::from(body)))
        .map_err(|_| "invalid Visor publication request")?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|_| "Visor publication request failed")?;
    match response.status() {
        hyper::StatusCode::OK => {}
        hyper::StatusCode::FORBIDDEN => {
            return Err("current completed Squad delivery and local consent required")
        }
        hyper::StatusCode::CONFLICT => return Err("Squad artifact was refused"),
        hyper::StatusCode::SERVICE_UNAVAILABLE => {
            return Err("Squad reporter unavailable, retry with the same draft")
        }
        _ => return Err("invalid Squad publication request"),
    }
    Limited::new(response.into_body(), 1024)
        .collect()
        .await
        .map(|body| body.to_bytes())
        .map_err(|_| "invalid Visor publication response")
}

pub(super) fn run(
    args: impl Iterator<Item = String>,
    input: impl Read,
    mut output: impl Write,
) -> ExitCode {
    let run = || {
        let body = request_body(args, input)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "could not start Visor publication client")?;
        let result = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(15), publish(body))
                .await
                .map_err(|_| "publication reply timed out, retry with the same draft")?
        })?;
        output
            .write_all(&result)
            .and_then(|()| output.write_all(b"\n"))
            .map_err(|_| "could not write publication IDs")
    };
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hypermesh-visor: {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attaches_plane_scope_without_rewriting_artifact() {
        let task = Uuid::new_v4();
        let artifact = json!({"kind": "brief", "draft": {"markdown": "10 seats for 30 days"}});
        let input = serde_json::to_vec(&artifact).unwrap();
        let result = request_body([task.to_string()].into_iter(), input.as_slice()).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&result).unwrap(),
            json!({
                "planeTaskId": task, "artifact": artifact,
            })
        );
    }

    #[test]
    fn refuses_extra_arguments_and_oversized_frames() {
        let task = Uuid::new_v4().to_string();
        assert!(request_body([task.clone(), "extra".into()].into_iter(), &b"{}"[..]).is_err());
        let oversized = vec![b' '; MAX_DRAFT_BYTES as usize + 1];
        assert!(request_body([task].into_iter(), oversized.as_slice()).is_err());
    }
}
