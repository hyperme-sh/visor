use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

const MAX_BEARER_BYTES: usize = 8192;
const MAX_CONTEXT_BYTES: usize = 16 * 1024;
const MAX_ARTIFACT_BYTES: usize = 384 * 1024;
const MAX_CURSOR: u64 = (1 << 53) - 1;
const USAGE: &str = "usage: hypermesh-visor squad-resume HTTPS_ORIGIN FORUM PLANE_TASK_ID AGENT [CURSOR SQUAD_TASK_ID [--sources BRIEF_REVISION WORKBOOK_REVISION [--preview-base PREVIEW_REVISION]]]";

struct ResumeRequest {
    url: Url,
    forum: String,
    delivery: String,
    agent: String,
    cursor: u64,
    task: Option<String>,
    sources: Option<RevisionSources>,
}

struct RevisionSources {
    brief: String,
    workbook: String,
    preview_base: Option<String>,
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
            return Err(USAGE);
        };
        let (cursor, task, sources) = match tail {
            [] => (0, None, None),
            [cursor, task] if canonical_id(task) => (
                cursor
                    .parse::<u64>()
                    .ok()
                    .filter(|&value| value <= MAX_CURSOR)
                    .ok_or("invalid Squad cursor")?,
                Some(task.clone()),
                None,
            ),
            [zero, task, flag, brief, workbook, preview @ ..]
                if zero == "0"
                    && flag == "--sources"
                    && canonical_id(task)
                    && canonical_id(brief)
                    && canonical_id(workbook) =>
            {
                let preview_base = match preview {
                    [] => None,
                    [flag, revision] if flag == "--preview-base" && canonical_id(revision) => {
                        Some(revision.clone())
                    }
                    _ => return Err(USAGE),
                };
                (
                    0,
                    Some(task.clone()),
                    Some(RevisionSources {
                        brief: brief.clone(),
                        workbook: workbook.clone(),
                        preview_base,
                    }),
                )
            }
            _ => return Err(USAGE),
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
            task,
            sources,
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
        let value = bounded_json(response, MAX_CONTEXT_BYTES)?;
        let cursor = value["cursor"].as_u64().ok_or("invalid Squad cursor")?;
        let valid_task = value["taskId"].as_str().is_some_and(|task| {
            canonical_id(task) && self.task.as_deref().is_none_or(|expected| task == expected)
        });
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

    fn post(
        &self,
        session: &ureq::Agent,
        authorization: &str,
        body: Value,
    ) -> Result<ureq::Response, &'static str> {
        let response = session
            .post(self.url.as_str())
            .set("Authorization", authorization)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .map_err(|_| "Squad refused the enrolled task or is unavailable")?;
        if response.status() != 200 {
            return Err("Squad refused the enrolled task or is unavailable");
        }
        Ok(response)
    }

    fn artifact_body(&self, action: &str, task: &str, revision: &str) -> Value {
        json!({"action":action, "forum":self.forum, "taskId":task, "revisionId":revision})
    }

    fn sources(
        &self,
        session: &ureq::Agent,
        authorization: &str,
        context: Value,
    ) -> Result<Value, &'static str> {
        let Some(sources) = &self.sources else {
            return Ok(context);
        };
        let task = self.task.as_deref().ok_or("missing Squad source task")?;
        let brief = self.post(
            session,
            authorization,
            self.artifact_body("brief-read", task, &sources.brief),
        )?;
        let brief = selected_revision(
            bounded_json(brief.into_reader(), MAX_ARTIFACT_BYTES)?,
            "brief",
            task,
            &sources.brief,
        )?;
        let workbook = self.post(
            session,
            authorization,
            self.artifact_body("workbook-read", task, &sources.workbook),
        )?;
        let workbook = selected_revision(
            bounded_json(workbook.into_reader(), MAX_ARTIFACT_BYTES)?,
            "workbook",
            task,
            &sources.workbook,
        )?;
        let mut result = json!({"context":context, "brief":brief, "workbook":workbook});
        if let Some(revision) = &sources.preview_base {
            let prior = self.post(
                session,
                authorization,
                self.artifact_body("preview-read", task, revision),
            )?;
            let prior = bounded_json(prior.into_reader(), MAX_ARTIFACT_BYTES)?;
            result["previewBase"] = preview_base(&prior, task, revision)?;
        }
        Ok(result)
    }
}

fn preview_base(view: &Value, task: &str, revision: &str) -> Result<Value, &'static str> {
    let preview = &view["preview"];
    let id = preview["id"].as_str().filter(|id| canonical_id(id));
    if preview["taskId"].as_str() != Some(task)
        || preview["selected"]["id"].as_str() != Some(revision)
        || preview["current"].as_str() != Some(revision)
        || id.is_none()
    {
        return Err("Squad preview base differs from the current handoff");
    }
    Ok(json!({"artifactId": id, "revisionId": revision}))
}

fn bounded_json(response: impl Read, limit: usize) -> Result<Value, &'static str> {
    let mut bytes = Vec::new();
    response
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "could not read the Squad response")?;
    if bytes.len() > limit {
        return Err("Squad response exceeds its limit");
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid Squad response")
}

fn selected_revision(
    mut view: Value,
    kind: &str,
    task: &str,
    revision: &str,
) -> Result<Value, &'static str> {
    let artifact = view
        .get_mut(kind)
        .ok_or("Squad source revision unavailable")?;
    if artifact["taskId"].as_str() != Some(task)
        || artifact["selected"]["id"].as_str() != Some(revision)
        || !artifact["id"].as_str().is_some_and(canonical_id)
    {
        return Err("Squad source revision differs from the handoff");
    }
    let valid_content = if kind == "workbook" {
        artifact["selected"]["inputs"].is_object()
            && artifact["sheet"].as_str() == Some("Pricing")
            && artifact["totalCell"].as_str() == Some("B5")
            && artifact["totalCents"].as_u64().is_some()
    } else {
        artifact["selected"]["markdown"].is_string()
    };
    if !valid_content {
        return Err("invalid Squad source content");
    }
    let mut selected = json!({
        "id": artifact["id"],
        "selected": artifact["selected"].take(),
    });
    if kind == "workbook" {
        selected["sheet"] = artifact["sheet"].take();
        selected["totalCell"] = artifact["totalCell"].take();
        selected["totalCents"] = artifact["totalCents"].take();
    }
    Ok(selected)
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
        let session = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout(Duration::from_secs(15))
            .build();
        let response = request.post(&session, authorization.as_str(), request.body())?;
        let context = request.context(response.into_reader())?;
        let context = request.sources(&session, authorization.as_str(), context)?;
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
        assert!(ResumeRequest::parse(&args).is_err());
        args.push(TASK.into());
        let request = ResumeRequest::parse(&args).unwrap();
        let value = json!({"taskId":TASK,"cursor":4,"changed":false});
        assert_eq!(
            request.context(value.to_string().as_bytes()).unwrap(),
            value
        );
        let wrong_task = json!({"taskId":DELIVERY,"cursor":4,"changed":false});
        assert!(request.context(wrong_task.to_string().as_bytes()).is_err());
    }

    #[test]
    fn a_fresh_handoff_read_can_require_the_original_squad_task() {
        let mut args = args("https://squad.example.test");
        args.extend(["0".into(), TASK.into()]);
        let request = ResumeRequest::parse(&args).unwrap();
        let mut response = json!({"changed":true, "cursor":4, "taskId":TASK,
            "planeTaskId":DELIVERY, "delegate":request.agent});
        assert!(request.context(response.to_string().as_bytes()).is_ok());
        response["taskId"] = json!(DELIVERY);
        assert!(request.context(response.to_string().as_bytes()).is_err());
    }

    #[test]
    fn reads_only_explicit_revisions_from_the_original_task() {
        let mut args = args("https://squad.example.test");
        args.extend([
            "0".into(),
            TASK.into(),
            "--sources".into(),
            TASK.into(),
            DELIVERY.into(),
        ]);
        let request = ResumeRequest::parse(&args).unwrap();
        let sources = request.sources.as_ref().unwrap();
        assert_eq!(request.task.as_deref(), Some(TASK));
        assert_eq!(
            (sources.brief.as_str(), sources.workbook.as_str()),
            (TASK, DELIVERY)
        );
        assert!(sources.preview_base.is_none());
        assert_eq!(
            request.artifact_body("workbook-read", TASK, DELIVERY),
            json!({
                "action":"workbook-read","forum":"forum_partner_fixture",
                "taskId":TASK,"revisionId":DELIVERY,
            })
        );
        args[4] = "3".into();
        assert!(ResumeRequest::parse(&args).is_err());
    }

    #[test]
    fn checks_current_preview_base_from_the_original_task() {
        let mut args = args("https://squad.example.test");
        args.extend(
            [
                "0",
                TASK,
                "--sources",
                TASK,
                DELIVERY,
                "--preview-base",
                DELIVERY,
            ]
            .map(String::from),
        );
        let request = ResumeRequest::parse(&args).unwrap();
        assert_eq!(
            request.sources.unwrap().preview_base.as_deref(),
            Some(DELIVERY)
        );
        args.pop();
        assert!(ResumeRequest::parse(&args).is_err());

        let mut response = json!({"preview": {
            "id": TASK, "taskId": TASK, "current": DELIVERY,
            "selected": {"id": DELIVERY, "html": "<p>Prior one-pager</p>"},
        }});
        assert_eq!(
            preview_base(&response, TASK, DELIVERY).unwrap(),
            json!({"artifactId": TASK, "revisionId": DELIVERY})
        );
        assert!(preview_base(&response, DELIVERY, DELIVERY).is_err());
        response["preview"]["current"] = json!(TASK);
        assert!(preview_base(&response, TASK, DELIVERY).is_err());
    }

    #[test]
    fn returns_selected_sources_and_rejects_another_task_or_revision() {
        let response = json!({"workbook":{"id":TASK,"taskId":TASK,"selected":{"id":DELIVERY,"inputs":{}},
            "sheet":"Pricing","totalCell":"B5","totalCents":108000,"comments":["extra context"]}});
        assert_eq!(
            selected_revision(response.clone(), "workbook", TASK, DELIVERY).unwrap(),
            json!({
                "id":TASK,"selected":{"id":DELIVERY,"inputs":{}},"sheet":"Pricing","totalCell":"B5","totalCents":108000,
            })
        );
        assert!(selected_revision(response.clone(), "workbook", DELIVERY, DELIVERY).is_err());
        assert!(selected_revision(response, "workbook", TASK, TASK).is_err());
        assert!(selected_revision(json!(false), "brief", TASK, TASK).is_err());
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
