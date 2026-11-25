use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

const MAX_BEARER_BYTES: usize = 8192;
const MAX_CONTEXT_BYTES: usize = 16 * 1024;
const MAX_CURSOR: u64 = (1 << 53) - 1;

struct ResumeRequest {
    url: Url,
    forum: String,
    delivery: String,
    agent: String,
    cursor: u64,
}

fn canonical_id(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|id| !id.is_nil() && id.to_string() == value)
}

fn identity_part(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

impl ResumeRequest {
    fn parse(args: &[String]) -> Result<Self, &'static str> {
        let [origin, forum, delivery, agent, tail @ ..] = args else {
            return Err("usage: hypermesh-visor squad-resume HTTPS_ORIGIN FORUM PLANE_TASK_ID AGENT [CURSOR]");
        };
        let cursor = match tail {
            [] => 0,
            [cursor] => cursor
                .parse::<u64>()
                .ok()
                .filter(|&value| value <= MAX_CURSOR)
                .ok_or("invalid Squad cursor")?,
            _ => return Err("unexpected Squad resume arguments"),
        };
        let mut url = Url::parse(origin).map_err(|_| "invalid Squad origin")?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("Squad resume requires a credential-free HTTPS origin");
        }
        url.set_path("/api/squad/pilot/project");

        let valid_forum = forum.strip_prefix("forum_").is_some_and(|key| {
            !key.is_empty()
                && key.len() <= 249
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        });
        let valid_agent = agent
            .split_once('.')
            .is_some_and(|(key, suffix)| identity_part(key) && identity_part(suffix));
        if !valid_forum || !canonical_id(delivery) || !valid_agent {
            return Err("invalid Squad forum, Plane task, or enrolled agent");
        }
        Ok(Self {
            url,
            forum: forum.clone(),
            delivery: delivery.clone(),
            agent: agent.clone(),
            cursor,
        })
    }

    fn body(&self) -> Value {
        json!({
            "action": "resume",
            "forum": self.forum,
            "planeTaskId": self.delivery,
            "since": self.cursor
        })
    }

    fn context(&self, response: impl Read) -> Result<Value, &'static str> {
        let mut bytes = Vec::new();
        response
            .take(MAX_CONTEXT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "could not read Squad context")?;
        if bytes.len() > MAX_CONTEXT_BYTES {
            return Err("Squad context exceeds the response limit");
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| "invalid Squad context")?;
        let cursor = value["cursor"].as_u64().ok_or("invalid Squad cursor")?;
        let valid_task = value["taskId"].as_str().is_some_and(canonical_id);
        let bound = match value["changed"].as_bool() {
            Some(false) => {
                cursor == self.cursor
                    && cursor != 0
                    && value["planeTaskId"].is_null()
                    && value["delegate"].is_null()
            }
            Some(true) => {
                cursor > self.cursor
                    && value["planeTaskId"].as_str() == Some(self.delivery.as_str())
                    && value["delegate"].as_str() == Some(self.agent.as_str())
            }
            None => false,
        };
        if cursor > MAX_CURSOR || !valid_task || !bound {
            return Err("Squad context differs from the enrolled task");
        }
        Ok(value)
    }
}

fn read_bearer(input: impl Read) -> Result<Zeroizing<String>, &'static str> {
    let mut token = Zeroizing::new(String::new());
    input
        .take(MAX_BEARER_BYTES as u64 + 3)
        .read_to_string(&mut token)
        .map_err(|_| "could not read agent session from stdin")?;
    let length = token.trim_end_matches(['\r', '\n']).len();
    if token.len() > MAX_BEARER_BYTES + 2
        || length == 0
        || length > MAX_BEARER_BYTES
        || !token[..length]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        return Err("invalid agent session");
    }
    token.truncate(length);
    Ok(token)
}

pub(super) fn run(
    args: impl Iterator<Item = String>,
    input: impl Read,
    mut out: impl Write,
) -> ExitCode {
    let request = match ResumeRequest::parse(&args.collect::<Vec<_>>()) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("hypermesh-visor: {error}");
            return ExitCode::from(2);
        }
    };
    let result = (|| {
        let token = read_bearer(input)?;
        let authorization = Zeroizing::new(format!("Bearer {}", token.as_str()));
        let response = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout(Duration::from_secs(15))
            .build()
            .post(request.url.as_str())
            .set("Authorization", authorization.as_str())
            .set("Content-Type", "application/json")
            .send_string(&request.body().to_string())
            .map_err(|_| "Squad refused the enrolled task or is unavailable")?;
        if response.status() != 200 {
            return Err("Squad refused the enrolled task or is unavailable");
        }
        let context = request.context(response.into_reader())?;
        serde_json::to_writer(&mut out, &context).map_err(|_| "could not write Squad context")?;
        writeln!(out).map_err(|_| "could not write Squad context")
    })();
    match result {
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

    const TASK: &str = "9f9e472b-07d2-4fb9-a24a-31244ed7daba";
    const DELIVERY: &str = "727c899b-e8c4-8581-b9b1-6b795c2d11e1";

    fn args(origin: &str) -> Vec<String> {
        vec![
            origin.into(),
            "forum_partner_fixture".into(),
            DELIVERY.into(),
            format!("{}.{}", "a".repeat(43), "b".repeat(43)),
        ]
    }

    #[test]
    fn uses_only_the_configured_origin_and_exact_delivery() {
        let request = ResumeRequest::parse(&args("https://squad.example.test")).unwrap();
        assert_eq!(
            request.url.as_str(),
            "https://squad.example.test/api/squad/pilot/project"
        );
        assert_eq!(
            request.body(),
            json!({"action":"resume", "forum":"forum_partner_fixture", "planeTaskId":DELIVERY, "since":0})
        );
        for origin in [
            "http://squad.example.test",
            "https://name:token@squad.example.test",
            "https://squad.example.test/wrong",
            "https://squad.example.test/?query=x",
        ] {
            assert!(ResumeRequest::parse(&args(origin)).is_err());
        }
    }

    #[test]
    fn rejects_mismatched_delivery_principal_or_cursor() {
        let request = ResumeRequest::parse(&args("https://squad.example.test")).unwrap();
        let context = json!({"changed":true, "cursor":4, "taskId":TASK,
            "planeTaskId":DELIVERY, "delegate":request.agent, "branch":[], "sources":[]});
        let read = |value: &Value| request.context(value.to_string().as_bytes());
        assert_eq!(read(&context).unwrap(), context);
        for (key, value) in [
            ("planeTaskId", json!(TASK)),
            ("delegate", json!("another agent")),
            ("taskId", json!("wrong task")),
            ("cursor", json!(0)),
            ("cursor", json!(MAX_CURSOR + 1)),
        ] {
            let mut altered = context.clone();
            altered[key] = value;
            assert!(read(&altered).is_err(), "{key}");
        }
        assert!(request
            .context(&vec![b' '; MAX_CONTEXT_BYTES + 1][..])
            .is_err());
    }

    #[test]
    fn accepts_an_unchanged_response_at_the_saved_cursor() {
        let mut args = args("https://squad.example.test");
        args.push("4".into());
        let request = ResumeRequest::parse(&args).unwrap();
        let value = json!({"taskId":TASK,"cursor":4,"changed":false});
        assert_eq!(
            request.context(value.to_string().as_bytes()).unwrap(),
            value
        );
    }

    #[test]
    fn bounds_stdin_and_rejects_non_bearer_characters() {
        assert_eq!(
            read_bearer(&b"agent.fixture\n"[..]).unwrap().as_str(),
            "agent.fixture"
        );
        for input in ["", "token\nsecond-token", " bearer", "token\t"] {
            assert!(read_bearer(input.as_bytes()).is_err());
        }
        assert!(read_bearer("a".repeat(MAX_BEARER_BYTES + 1).as_bytes()).is_err());
    }
}
