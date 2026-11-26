use http_body_util::{BodyExt, Full, Limited};
use hyper::{body::Bytes, client::conn::http1, Method, Request};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{io::Write, process::ExitCode, time::Duration};
use uuid::Uuid;

const USAGE: &str = "Usage: hypermesh-visor github-issue enable SESSION PLUGIN REVISION ACCOUNT OWNER/REPO | status SESSION PLUGIN | read SESSION PLUGIN ACCOUNT OWNER/REPO NUMBER | revoke SESSION PLUGIN";
const UNAVAILABLE: &str = "Visor companion socket unavailable";

pub(crate) fn run(args: impl IntoIterator<Item = String>, mut output: impl Write) -> ExitCode {
    let args: Vec<_> = args.into_iter().collect();
    if matches!(args.as_slice(), [help] if help == "--help" || help == "-h") {
        return match writeln!(output, "{USAGE}") {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }
    match request(&args).and_then(|request| {
        if hypermesh_visor::current_euid() == 0 {
            return Err("Run as the owner of the Visor session");
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| UNAVAILABLE)?;
        runtime.block_on(send(request))
    }) {
        Ok((success, value)) => {
            if serde_json::to_writer_pretty(&mut output, &value).is_ok()
                && writeln!(output).is_ok()
                && success
            {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(message) => {
            eprintln!("hypermesh-visor github-issue: {message}");
            ExitCode::FAILURE
        }
    }
}

fn request(args: &[String]) -> Result<Request<Full<Bytes>>, &'static str> {
    let [command, session, package, rest @ ..] = args else {
        return Err(USAGE);
    };
    let session = Uuid::parse_str(session).map_err(|_| "Invalid session ID")?;
    if package.is_empty()
        || package.len() > 127
        || !package
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
    {
        return Err("Invalid plugin ID");
    }
    let base = format!("/session/{session}/plugins");
    let issue = format!("{base}/{package}/github-issue");
    let target = |account: &str, repository: &str| {
        let account: u64 = account
            .parse()
            .map_err(|_| "Invalid numeric GitHub account ID")?;
        Ok::<_, &'static str>(json!({"account":account,"repository":repository}))
    };
    let (method, path, body) = match (command.as_str(), rest) {
        ("enable", [revision, account, repository]) => (
            Method::POST,
            base,
            json!({
                "plugin_id":package,
                "revision":Uuid::parse_str(revision).map_err(|_| "Invalid plugin revision")?,
                "tool":"read-issue",
                "target":target(account, repository)?,
            }),
        ),
        ("status", []) => (Method::GET, issue, Value::Null),
        ("read", [account, repository, number]) => (
            Method::POST,
            issue,
            json!({
                "target":target(account, repository)?,
                "number":number.parse::<u64>().map_err(|_| "Invalid issue number")?,
            }),
        ),
        ("revoke", []) => (Method::DELETE, format!("{base}/{package}"), Value::Null),
        _ => return Err(USAGE),
    };
    let body = if body.is_null() {
        Bytes::new()
    } else {
        Bytes::from(body.to_string())
    };
    Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("connection", "close")
        .body(Full::new(body))
        .map_err(|_| "Invalid local plugin request")
}

async fn send(request: Request<Full<Bytes>>) -> Result<(bool, Value), &'static str> {
    let path = hypermesh_visor::runtime_dir()
        .map_err(|_| UNAVAILABLE)?
        .join("companion.sock");
    let endpoint = hypermesh_local_transport::Endpoint::unix(path).map_err(|_| UNAVAILABLE)?;
    tokio::time::timeout(Duration::from_secs(12), async move {
        let stream = hypermesh_local_transport::connect(&endpoint)
            .await
            .map_err(|_| UNAVAILABLE)?;
        let (mut client, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|_| UNAVAILABLE)?;
        tokio::spawn(connection);
        let response = client
            .send_request(request)
            .await
            .map_err(|_| UNAVAILABLE)?;
        let status = response.status();
        if status == hyper::StatusCode::NO_CONTENT {
            return Ok((true, json!({"state":"revoked"})));
        }
        let bytes = Limited::new(response.into_body(), 128 * 1024)
            .collect()
            .await
            .map_err(|_| UNAVAILABLE)?
            .to_bytes();
        let value = serde_json::from_slice(&bytes).map_err(|_| "Invalid response from Visor")?;
        Ok((status.is_success(), value))
    })
    .await
    .map_err(|_| UNAVAILABLE)?
}
