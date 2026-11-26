//! Explicit, digest-reviewed installations over the existing private socket.
use serde_json::{json, Value};

const USAGE: &str = "hypermesh-companion --plugins list\n\
  --plugins inspect SOURCE\n\
  --plugins install SOURCE PUBLICATION_SHA256\n\
  --plugins update SOURCE PUBLICATION_SHA256 INSTALLED_REVISION\n\
  --plugins enable ID INSTALLED_REVISION\n\
  --plugins disable ID INSTALLED_REVISION\n\
  --plugins remove ID INSTALLED_REVISION\n\
  --plugins grant SESSION ID INSTALLED_REVISION TOOL TARGET";

pub fn run(args: Vec<String>) -> Result<(), String> {
    let parts: Vec<&str> = args.iter().map(String::as_str).collect();
    let (method, path, body) = match parts.as_slice() {
        ["list"] => ("GET", "/plugins".into(), Value::Null),
        ["inspect", source] => ("POST", "/plugins/inspect".into(), json!({"source":source})),
        ["install", source, digest] => (
            "POST",
            "/plugins".into(),
            json!({
                "source":source,"publication_sha256":digest,"expected_revision":null
            }),
        ),
        ["update", source, digest, revision] => (
            "POST",
            "/plugins".into(),
            json!({
                "source":source,"publication_sha256":digest,"expected_revision":revision
            }),
        ),
        [action @ ("enable" | "disable"), id, revision] if valid_id(id) => (
            "PUT",
            format!("/plugins/{id}"),
            json!({"expected_revision":revision,"enabled":*action=="enable"}),
        ),
        ["remove", id, revision] if valid_id(id) => (
            "DELETE",
            format!("/plugins/{id}"),
            json!({"expected_revision":revision}),
        ),
        ["grant", session, id, revision, tool, target]
            if valid_id(id) && uuid::Uuid::parse_str(session).is_ok() =>
        {
            let session = uuid::Uuid::parse_str(session).map_err(|_| "Invalid session")?;
            (
                "POST",
                format!("/session/{session}/plugins"),
                json!({
                    "plugin_id":id,"revision":revision,"tool":tool,"target":target
                }),
            )
        }
        ["--help" | "-h"] => {
            println!("{USAGE}");
            return Ok(());
        }
        _ => return Err(USAGE.into()),
    };
    let result = crate::visor::plugin_request(method, &path, &body)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&result).map_err(|_| "Invalid plugin response")?
    );
    Ok(())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 127
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-".contains(&b))
}
