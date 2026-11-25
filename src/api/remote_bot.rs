//! Opt-in outbound bot polling. Remote control never grants native consent.
use super::*;
use serde_json::{json, Value};
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    origin: String,
    tenant_id: Uuid,
    bot_id: Uuid,
    session_id: Uuid,
    mode: String,
    credential_file: PathBuf,
    #[serde(default)]
    forum_prompt: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    forum_reporter_socket: Option<PathBuf>,
}

pub(super) struct Client {
    settings: Settings,
    config_path: PathBuf,
    credential: zeroize::Zeroizing<String>,
    connection: Uuid,
    agent: ureq::Agent,
}

fn private_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let file =
        hypermesh_local_storage::open(path, hypermesh_local_storage::Open::Read { private: true })
            .map_err(|_| "bot configuration cannot be opened")?;
    let meta = file
        .metadata()
        .map_err(|_| "bot configuration cannot be inspected")?;
    if !meta.is_file() || meta.len() > 8192 {
        return Err("bot configuration must be an owner-only regular file under 8 KiB".into());
    }
    let mut data = Vec::new();
    file.take(8193)
        .read_to_end(&mut data)
        .map_err(|_| "bot configuration cannot be read")?;
    if data.len() > 8192 {
        return Err("bot configuration is too large".into());
    }
    Ok(data)
}

fn validate_origin(value: &str) -> Result<String, String> {
    let url = url::Url::parse(value).map_err(|_| "invalid bot service origin")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("bot service must be a credential-free HTTPS origin".into());
    }
    Ok(url.origin().ascii_serialization())
}

pub(super) fn configured() -> Result<Option<Arc<Client>>, String> {
    let Some(path) = std::env::var_os("HYPERMESH_BOT_CONFIG") else {
        return Ok(None);
    };
    let mut settings: Settings =
        serde_json::from_slice(&private_file(std::path::Path::new(&path))?)
            .map_err(|_| "invalid bot configuration")?;
    settings.origin = validate_origin(&settings.origin)?;
    if !matches!(settings.mode.as_str(), "interactive" | "container")
        || !settings.credential_file.is_absolute()
        || settings
            .forum_reporter_socket
            .as_ref()
            .is_some_and(|path| !cfg!(unix) || !path.is_absolute())
    {
        return Err("bot mode and absolute credential file are required".into());
    }
    let bytes = zeroize::Zeroizing::new(private_file(&settings.credential_file)?);
    let credential = std::str::from_utf8(&bytes)
        .map_err(|_| "invalid bot credential")?
        .trim();
    if credential.len() != 71
        || !credential.starts_with("hm_bot_")
        || !credential[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid scoped bot credential".into());
    }
    Ok(Some(Arc::new(Client {
        config_path: path.into(),
        settings,
        credential: zeroize::Zeroizing::new(credential.into()),
        connection: Uuid::new_v4(),
        agent: ureq::AgentBuilder::new()
            .redirects(0)
            .timeout(Duration::from_secs(2))
            .build(),
    })))
}

impl Client {
    fn bound_session(&self) -> Result<Uuid, String> {
        let updated: Settings = serde_json::from_slice(&private_file(&self.config_path)?)
            .map_err(|_| "invalid bot configuration")?;
        if validate_origin(&updated.origin)? != self.settings.origin
            || updated.tenant_id != self.settings.tenant_id
            || updated.bot_id != self.settings.bot_id
            || updated.mode != self.settings.mode
            || updated.credential_file != self.settings.credential_file
            || updated.forum_prompt != self.settings.forum_prompt
            || updated.forum_reporter_socket != self.settings.forum_reporter_socket
        {
            return Err("bot identity configuration changed; restart required".into());
        }
        Ok(updated.session_id)
    }

    async fn post(self: &Arc<Self>, action: &'static str, body: Value) -> Result<Value, String> {
        let client = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let target = format!(
                "{}/api/v1/hypermesh/bots/{}/agent/{action}",
                client.settings.origin, client.settings.bot_id
            );
            let response = client
                .agent
                .post(&target)
                .set("Authorization", &format!("Bearer {}", &*client.credential))
                .set("X-Tenant-ID", &client.settings.tenant_id.to_string())
                .set("Content-Type", "application/json")
                .send_string(&body.to_string())
                .map_err(|_| "bot service request failed")?;
            if response.status() != 200 {
                return Err("bot service refused request".into());
            }
            let mut bytes = Vec::new();
            response
                .into_reader()
                .take(1_048_577)
                .read_to_end(&mut bytes)
                .map_err(|_| "bot response read failed")?;
            if bytes.len() > 1_048_576 {
                return Err("bot response too large".to_string());
            }
            serde_json::from_slice(&bytes).map_err(|_| "invalid bot response".to_string())
        })
        .await
        .map_err(|_| "bot request worker failed")?
    }

    fn status_matches(&self, value: &Value) -> bool {
        value["bot_id"].as_str() == Some(self.settings.bot_id.to_string().as_str())
            && value["tenant_id"].as_str() == Some(self.settings.tenant_id.to_string().as_str())
            && value["mode"].as_str() == Some(self.settings.mode.as_str())
            && value["revoked"] == false
    }
}

fn task_binding(task: &Value, bot: &Value, session: Uuid) -> bool {
    task["session_id"].as_str() == Some(session.to_string().as_str())
        && bot["assignment"]["session_id"] == task["session_id"]
        && task["assignment_id"].is_string()
        && bot["assignment"]["id"] == task["assignment_id"]
        && task["status"] == "delivered"
        && Uuid::parse_str(task["id"].as_str().unwrap_or("")).is_ok()
}

fn remote_forum_prompt(task: &Value) -> bool {
    let Some(payload) = task["payload"].as_object() else {
        return false;
    };
    task["capability"] == "prompt"
        && delivery_id(task).is_some()
        && task["forum"]["kind"] == "remote-agent"
        && task["forum"]["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty() && id.len() <= 256)
        && task["forum"]["source_task_id"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok())
            .is_some_and(|id| !id.is_nil())
        && payload.len() == 2
        && task["payload"]["version"] == 1
        && task["payload"]["request_sha256"]
            .as_str()
            .is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
}

fn delivery_id(task: &Value) -> Option<Uuid> {
    Uuid::parse_str(task["delivery_id"].as_str()?)
        .ok()
        .filter(|id| !id.is_nil())
}

struct RemoteClaim {
    task: Uuid,
    delivery: Uuid,
    consent: tasks::Consent,
}

fn remote_delivered(status: &Value, claim: &RemoteClaim) -> bool {
    status["desired"] == "running"
        && status["tasks"].as_array().is_some_and(|tasks| {
            tasks.iter().any(|t| {
                t["id"] == claim.task.to_string()
                    && delivery_id(t) == Some(claim.delivery)
                    && task_binding(t, status, claim.consent.session)
                    && t["cancel_requested"] != true
            })
        })
}

fn remaining(deadline: f64, now: f64) -> Result<Duration, String> {
    let seconds = deadline - now;
    if !deadline.is_finite()
        || !now.is_finite()
        || !(0.0..=15.0).contains(&seconds)
        || seconds == 0.0
    {
        return Err("bot authorization expired or clock differs".into());
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn may_claim(status: &Value, session: Uuid, consent: bool) -> bool {
    consent
        && !session.is_nil()
        && status["desired"] == "running"
        && status["assignment"]["session_id"].as_str() == Some(session.to_string().as_str())
}

fn forum_report(
    status: &Value,
    task: &Value,
    session: Uuid,
    connection: Uuid,
    now: f64,
) -> Option<(Uuid, Value)> {
    let delivery = delivery_id(task)?;
    if status["desired"] != "running"
        || status["assignment"]["session_id"] != session.to_string()
        || status["assignment"]["id"] != task["assignment_id"]
        || task["session_id"] != session.to_string()
        || task["connection_id"] != connection.to_string()
        || task["cancel_requested"] == true
        || !matches!(task["status"].as_str(), Some("completed" | "failed"))
        || !matches!(
            task["forum"]["kind"].as_str(),
            Some("local" | "remote-agent")
        )
        || task["editor_deadline"]
            .as_f64()
            .is_none_or(|deadline| deadline <= now)
    {
        return None;
    }
    let forum = task["forum"]["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 256)?;
    let principal = task["forum"]["principal"]
        .as_str()
        .filter(|key| key.len() == 87)?;
    Some((
        delivery,
        json!({
            "tenant": status["tenant_id"], "botId": status["bot_id"], "assignmentId": task["assignment_id"],
            "sessionId": task["session_id"], "taskId": task["id"], "deliveryId": delivery,
            "forumId": forum, "principal": principal, "status": task["status"],
        }),
    ))
}

fn local_report_consent(saved: &Value, report: &Value) -> Option<tasks::Consent> {
    if saved["result"]["task_id"] != report["taskId"] || !report["taskId"].is_string() {
        return None;
    }
    let identity = &saved["identity"];
    for (field, projected) in [
        ("tenant_id", "tenant"),
        ("bot_id", "botId"),
        ("assignment_id", "assignmentId"),
        ("session_id", "sessionId"),
    ] {
        if !identity[field].is_string() || identity[field] != report[projected] {
            return None;
        }
    }
    Some(tasks::Consent {
        session: Uuid::parse_str(report["sessionId"].as_str()?).ok()?,
        word: saved["consent_word"].as_u64()?,
        resume_seq: saved["resume_seq"].as_u64()?,
    })
}

const REPORT_FRAME_BYTES: u64 = 65_536;

#[cfg(unix)]
async fn forum_exchange(path: &std::path::Path, mut body: Vec<u8>) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let checked = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let parent = checked
            .parent()
            .ok_or_else(|| std::io::Error::other("forum reporter directory unavailable"))?;
        let directory = std::fs::symlink_metadata(parent)?;
        let socket = std::fs::symlink_metadata(&checked)?;
        let owner = unsafe { libc::geteuid() };
        if !directory.is_dir()
            || directory.uid() != owner
            || directory.mode() & 0o077 != 0
            || !socket.file_type().is_socket()
            || socket.uid() != owner
        {
            return Err(std::io::Error::other(
                "private forum reporter socket required",
            ));
        }
        Ok(())
    })
    .await
    .map_err(std::io::Error::other)??;
    let mut stream = tokio::net::UnixStream::connect(path).await?;
    if !crate::companion_sock::peer_is_current_user(&stream) {
        return Err(std::io::Error::other(
            "private forum reporter peer required",
        ));
    }
    body.push(b'\n');
    stream.write_all(&body).await?;
    let mut reply = Vec::new();
    BufReader::new(stream.take(1024))
        .read_until(b'\n', &mut reply)
        .await?;
    if reply.last() != Some(&b'\n') {
        return Err(std::io::Error::other("invalid forum reporter reply"));
    }
    Ok(reply)
}

#[cfg(not(unix))]
async fn forum_exchange(_path: &std::path::Path, _body: Vec<u8>) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::other("forum reporter socket unavailable"))
}

async fn exchange_while_consented(
    path: &std::path::Path,
    body: Vec<u8>,
    deadline: Instant,
    state: &AppState,
    permit: tasks::Consent,
) -> Result<Vec<u8>, ApiError> {
    let request = tokio::time::timeout_at(deadline.into(), async {
        let _exchange = state.forum_reporter_exchange.lock().await;
        if !permit.matches(&lock(&state.automation)) {
            return Err(ApiError::forbidden("Delivery consent is no longer current"));
        }
        forum_exchange(path, body)
            .await
            .map_err(|_| ApiError::unavailable("Forum reporter unavailable"))
    });
    tokio::pin!(request);
    let mut guard = tokio::time::interval(Duration::from_millis(100));
    loop {
        if !permit.matches(&lock(&state.automation)) {
            return Err(ApiError::forbidden("Delivery consent is no longer current"));
        }
        tokio::select! {
            result = &mut request => {
                if !permit.matches(&lock(&state.automation)) {
                    return Err(ApiError::forbidden("Delivery consent is no longer current"));
                }
                return result.unwrap_or_else(|_| Err(ApiError::unavailable("Forum reporter unavailable")));
            }
            _ = guard.tick() => {},
        }
    }
}

#[derive(Clone)]
pub(super) struct Publication {
    socket: PathBuf,
    report: Value,
    permit: tasks::Consent,
    deadline: Instant,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PublishRequest {
    plane_task_id: Uuid,
    artifact: Artifact,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    kind: ArtifactKind,
    draft: Value,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum ArtifactKind {
    Brief,
    Workbook,
    Preview,
}

#[derive(Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ArtifactResult {
    version: u32,
    task_id: Uuid,
    artifact_id: Uuid,
    revision_id: Uuid,
}

impl ArtifactResult {
    fn expected(draft: &Value) -> Result<Self, ApiError> {
        let id = |name: &str| {
            draft[name]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .filter(|id| !id.is_nil())
                .ok_or_else(|| ApiError::bad("Artifact requires task, artifact and revision IDs"))
        };
        Ok(Self {
            version: 1,
            task_id: id("taskId")?,
            artifact_id: id("artifactId")?,
            revision_id: id("revisionId")?,
        })
    }
}

pub(super) async fn publish(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Json(body): Json<PublishRequest>,
) -> Result<Json<ArtifactResult>, ApiError> {
    if !link.0 {
        return Err(ApiError::forbidden("Local companion socket required"));
    }
    let scope = lock(&state.forum_publications)
        .get(&body.plane_task_id)
        .cloned()
        .filter(|scope| scope.deadline > Instant::now())
        .ok_or_else(|| ApiError::forbidden("Current completed Squad delivery required"))?;
    let expected = ArtifactResult::expected(&body.artifact.draft)?;
    let frame = serde_json::to_vec(&json!({
        "version": 1, "report": scope.report, "artifact": body.artifact,
    }))
    .map_err(|_| ApiError::bad("Invalid Squad artifact"))?;
    if frame.len() as u64 >= REPORT_FRAME_BYTES {
        return Err(ApiError::bad("Squad artifact exceeds reporter frame limit"));
    }
    let deadline = scope.deadline.min(Instant::now() + Duration::from_secs(10));
    let reply =
        exchange_while_consented(&scope.socket, frame, deadline, &state, scope.permit).await?;
    let publication = serde_json::from_slice::<ArtifactResult>(&reply)
        .ok()
        .filter(|result| result == &expected)
        .ok_or_else(|| ApiError::conflict("Forum reporter refused the artifact"))?;
    Ok(Json(publication))
}

fn current_forum_publications(
    status: &Value,
    client: &Client,
    state: &AppState,
    session: Uuid,
    permits: &mut HashMap<Uuid, tasks::Consent>,
) -> Result<Vec<(Uuid, Publication)>, String> {
    let (Some(path), Some(items)) = (
        &client.settings.forum_reporter_socket,
        status["tasks"].as_array(),
    ) else {
        return Ok(Vec::new());
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "invalid system clock")?
        .as_secs_f64();
    let instant = Instant::now();
    Ok(items
        .iter()
        .filter_map(|task| {
            let (delivery, report) = forum_report(status, task, session, client.connection, now)?;
            let permit = permits.get(&delivery).copied().or_else(|| {
                if task["forum"]["kind"] != "local" {
                    return None;
                }
                let permit = local_report_consent(&lock(&state.coding_status), &report)?;
                permits.insert(delivery, permit);
                Some(permit)
            })?;
            let duration =
                Duration::try_from_secs_f64(task["editor_deadline"].as_f64()? - now).ok()?;
            let deadline = instant.checked_add(duration)?;
            permit.matches(&lock(&state.automation)).then_some((
                delivery,
                Publication {
                    socket: path.clone(),
                    report,
                    permit,
                    deadline,
                },
            ))
        })
        .collect())
}

pub(super) async fn run(
    client: Arc<Client>,
    state: Arc<AppState>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let result = poll(client, Arc::clone(&state), &mut stop).await;
    lock(&state.forum_publications).clear();
    if let Err(message) = result {
        lock(&state.automation).pause();
        return Err(std::io::Error::other(message));
    }
    Ok(())
}

async fn poll(
    client: Arc<Client>,
    state: Arc<AppState>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    let mut previous_session = client.settings.session_id;
    let mut active: Option<(String, String)> = None;
    let mut remote: Option<RemoteClaim> = None;
    let mut reported = std::collections::HashSet::new();
    let mut report_permits = std::collections::HashMap::new();
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        let session = client.bound_session()?;
        if (active.is_some() || remote.is_some()) && session != previous_session {
            if remote.is_some() {
                client
                    .post(
                        "heartbeat",
                        json!({"connection_id":client.connection,"local_consent":false}),
                    )
                    .await?;
            }
            return Err("bot session changed during active work".into());
        }
        previous_session = session;
        let has_session = state.has_session(session);
        let consent_word = {
            let gate = lock(&state.automation);
            (has_session && gate.phase() == Phase::Running && gate.session() == Some(session))
                .then(|| tasks::Consent::capture(&gate, session))
        };
        // A local pause/resume between polls must not renew the prior delivery.
        let consent = consent_word.is_some()
            && remote
                .as_ref()
                .is_none_or(|claim| claim.consent.matches(&lock(&state.automation)));
        let status = client
            .post(
                "heartbeat",
                json!({"connection_id":client.connection,"local_consent":consent}),
            )
            .await?;
        if !client.status_matches(&status) {
            return Err("bot registration binding differs".into());
        }
        if let Some(claim) = &remote {
            if !consent || !remote_delivered(&status, claim) {
                if status["desired"] != "running" {
                    lock(&state.automation).pause();
                }
                remote = None;
            }
        } else if let Some((cloud_id, local_id)) = &active {
            let cancelled = status["tasks"]
                .as_array()
                .and_then(|items| items.iter().find(|t| t["id"] == *cloud_id))
                .map(|t| {
                    t["cancel_requested"] == true
                        || t["status"] == "unknown"
                        || t["status"] == "cancelled"
                })
                .unwrap_or(true);
            if status["desired"] != "running" || !consent || cancelled {
                lock(&state.automation).pause();
                let _ = tasks::cancel(
                    State(Arc::clone(&state)),
                    Path((session.to_string(), local_id.clone())),
                )
                .await;
            }
            let Json(local) = tasks::status(
                State(Arc::clone(&state)),
                Path((session.to_string(), local_id.clone())),
            )
            .await
            .map_err(|_| "local bot task unavailable")?;
            if matches!(
                local["status"].as_str(),
                Some("completed" | "failed" | "cancelled")
            ) {
                client.post("complete", json!({"connection_id":client.connection,"task_id":cloud_id,"status":local["status"]})).await?;
                active = None;
            }
        } else if may_claim(&status, session, consent) {
            let mut claim_request = json!({"connection_id":client.connection});
            if client.settings.forum_prompt {
                claim_request["forum_prompt"] = json!(true);
            }
            let reply = client.post("claim", claim_request).await?;
            if !client.status_matches(&reply["bot"]) {
                return Err("bot claim binding differs".into());
            }
            if !reply["task"].is_null() {
                let task = &reply["task"];
                if !task_binding(task, &reply["bot"], session)
                    || task["connection_id"] != client.connection.to_string()
                {
                    return Err("bot task assignment differs".into());
                }
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| "invalid system clock")?
                    .as_secs_f64();
                let deadline = remaining(
                    reply["authorized_until"]
                        .as_f64()
                        .ok_or("missing bot deadline")?,
                    now,
                )?;
                let consent_word = consent_word.ok_or("local bot consent unavailable")?;
                if !consent_word.matches(&lock(&state.automation)) {
                    client
                        .post(
                            "heartbeat",
                            json!({"connection_id":client.connection,"local_consent":false}),
                        )
                        .await?;
                    return Err("local bot consent changed".into());
                }
                if client.settings.forum_prompt && remote_forum_prompt(task) {
                    let task_id = serde_json::from_value(task["id"].clone())
                        .map_err(|_| "invalid remote-agent task ID")?;
                    let delivery = delivery_id(task).ok_or("invalid forum delivery ID")?;
                    report_permits.insert(delivery, consent_word);
                    remote = Some(RemoteClaim {
                        task: task_id,
                        delivery,
                        consent: consent_word,
                    });
                } else if task["capability"] == "desktop" {
                    let body = serde_json::from_value(task["payload"].clone())
                        .map_err(|_| "invalid bot task payload")?;
                    let (_, Json(local)) = tasks::submit_bounded(
                        Arc::clone(&state),
                        session.to_string(),
                        body,
                        deadline,
                        Some(consent_word),
                    )
                    .await
                    .map_err(|_| "bot task refused locally")?;
                    active = Some((
                        task["id"].as_str().unwrap().into(),
                        local["id"].as_str().ok_or("missing local task ID")?.into(),
                    ));
                } else {
                    return Err("bot task capability or delivery differs".into());
                }
            }
        }
        let publications =
            current_forum_publications(&status, &client, &state, session, &mut report_permits)?;
        *lock(&state.forum_publications) = publications
            .iter()
            .filter_map(|(_, scope)| {
                if scope.report["status"] != "completed" {
                    return None;
                }
                let plane_task_id = Uuid::parse_str(scope.report["taskId"].as_str()?).ok()?;
                Some((plane_task_id, scope.clone()))
            })
            .collect();
        if let Some((delivery, scope)) = publications.iter().find(|(id, _)| !reported.contains(id))
        {
            let deadline = scope.deadline.min(Instant::now() + Duration::from_secs(5));
            let frame = scope.report.to_string().into_bytes();
            if consent
                && exchange_while_consented(&scope.socket, frame, deadline, &state, scope.permit)
                    .await
                    .is_ok_and(|reply| reply == b"ok\n")
            {
                reported.insert(*delivery);
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {},
            _ = async { let _ = stop.wait_for(|v| *v).await; } => {
                if remote.is_some() {
                    client.post("heartbeat", json!({"connection_id":client.connection,"local_consent":false})).await?;
                }
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn artifacts_use_the_completed_delivery_and_original_consent() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = std::env::temp_dir().join(format!("visor-artifact-{}", Uuid::new_v4()));
        crate::companion_sock::prepare_dir(&dir).unwrap();
        let desktop = crate::FixtureDesktop::open(&dir.join("desktop.jsonl")).unwrap();
        let state = Arc::new(AppState::new(Arc::new(desktop), Duration::from_millis(20)));
        let socket = dir.join("reporter.sock");
        let reporter = tokio::net::UnixListener::bind(&socket).unwrap();
        let session = Uuid::new_v4();
        let plane = Uuid::new_v4();
        let project = Uuid::new_v4();
        let artifact = Uuid::new_v4();
        let revision = Uuid::new_v4();
        let permit = {
            let mut gate = lock(&state.automation);
            gate.control(Some(session), automation::Control::Start)
                .unwrap();
            tasks::Consent::capture(&gate, session)
        };
        let report = json!({"taskId": plane, "sessionId": session, "status": "completed",
            "assignmentId": Uuid::new_v4(), "deliveryId": Uuid::new_v4()});
        lock(&state.forum_publications).insert(
            plane,
            Publication {
                socket,
                report: report.clone(),
                permit,
                deadline: Instant::now() + Duration::from_secs(10),
            },
        );
        let draft = json!({"taskId": project, "artifactId": artifact, "revisionId": revision,
            "base": null, "markdown": "10 seats for 30 days"});
        let body = json!({"kind": "brief", "draft": draft});
        let request = |task: Uuid| {
            Json(
                serde_json::from_value(json!({
                    "planeTaskId": task, "artifact": body,
                }))
                .unwrap(),
            )
        };
        let expected = json!({"version": 1, "taskId": project, "artifactId": artifact, "revisionId": revision});
        let publisher = tokio::spawn({
            let expected = expected.clone();
            let body = body.clone();
            async move {
                for attempt in 0..3 {
                    let (stream, _) = reporter.accept().await.unwrap();
                    let mut stream = BufReader::new(stream);
                    let mut frame = String::new();
                    stream.read_line(&mut frame).await.unwrap();
                    assert_eq!(
                        serde_json::from_str::<Value>(&frame).unwrap(),
                        json!({
                            "version": 1, "report": report, "artifact": body,
                        })
                    );
                    let mut response = expected.clone();
                    if attempt == 2 {
                        response["revisionId"] = json!(Uuid::new_v4());
                    }
                    stream
                        .get_mut()
                        .write_all(format!("{response}\n").as_bytes())
                        .await
                        .unwrap();
                }
            }
        });
        for (link, task) in [(false, plane), (true, Uuid::new_v4())] {
            assert!(matches!(
                publish(State(state.clone()), Extension(CompanionLink(link)), request(task)).await,
                Err(error) if error.status == StatusCode::FORBIDDEN,
            ));
        }
        for _ in 0..2 {
            let result = publish(
                State(state.clone()),
                Extension(CompanionLink(true)),
                request(plane),
            )
            .await
            .map_err(|error| error.message)
            .unwrap();
            assert_eq!(serde_json::to_value(result.0).unwrap(), expected);
        }
        assert!(matches!(
            publish(State(state.clone()), Extension(CompanionLink(true)), request(plane)).await,
            Err(error) if error.status == StatusCode::CONFLICT,
        ));
        publisher.await.unwrap();
        lock(&state.automation)
            .control(Some(session), automation::Control::Pause)
            .unwrap();
        lock(&state.automation)
            .control(Some(session), automation::Control::Start)
            .unwrap();
        assert!(matches!(
            publish(State(state.clone()), Extension(CompanionLink(true)), request(plane)).await,
            Err(error) if error.status == StatusCode::FORBIDDEN,
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn origin_rejects_plaintext_redirect_paths_and_embedded_credentials() {
        for origin in [
            "http://localhost",
            "https://user:secret@example.test",
            "https://example.test/path",
            "https://example.test/?token=x",
            "https://example.test/#x",
        ] {
            assert!(validate_origin(origin).is_err());
        }
        assert_eq!(
            validate_origin("https://example.test:8443/").unwrap(),
            "https://example.test:8443"
        );
    }
    #[test]
    fn restarted_container_waits_for_new_assignment_and_local_consent() {
        let old = Uuid::new_v4();
        let current = Uuid::new_v4();
        let mut status = json!({"desired":"running", "assignment":{"session_id":old}});
        assert!(!may_claim(&status, current, true));
        status["assignment"]["session_id"] = json!(current);
        assert!(!may_claim(&status, current, false));
        assert!(may_claim(&status, current, true));
        status["desired"] = json!("paused");
        assert!(!may_claim(&status, current, true));
        assert!(!may_claim(&json!({"desired":"running"}), current, true));
    }
    #[test]
    fn deadline_is_bounded_and_never_extended() {
        assert_eq!(remaining(110.0, 100.0).unwrap(), Duration::from_secs(10));
        for deadline in [100.0, 99.0, 116.0, f64::NAN, f64::INFINITY] {
            assert!(remaining(deadline, 100.0).is_err());
        }
    }
    #[test]
    fn task_requires_exact_local_session_and_remote_assignment() {
        let session = Uuid::new_v4();
        let assignment = Uuid::new_v4();
        let mut task = json!({"id":Uuid::new_v4(),"session_id":session,"assignment_id":assignment,"capability":"desktop","status":"delivered"});
        let bot = json!({"assignment":{"id":assignment,"session_id":session}});
        assert!(task_binding(&task, &bot, session));
        assert!(!task_binding(&task, &bot, Uuid::new_v4()));
        task["assignment_id"] = json!(Uuid::new_v4());
        assert!(!task_binding(&task, &bot, session));
    }
    #[test]
    fn remote_forum_claim_is_a_digest_only_prompt() {
        let mut task = json!({
            "capability": "prompt",
            "delivery_id": Uuid::new_v4(),
            "forum": {"kind": "remote-agent", "id": "fixture", "source_task_id": Uuid::new_v4()},
            "payload": {"version": 1, "request_sha256": "b".repeat(64)}
        });
        assert!(remote_forum_prompt(&task));
        task["payload"]["steps"] = json!([]);
        assert!(!remote_forum_prompt(&task));
        task["payload"].as_object_mut().unwrap().remove("steps");
        task["forum"]["kind"] = json!("local");
        assert!(!remote_forum_prompt(&task));
    }
    #[test]
    fn forum_report_only_projects_terminal_status_from_current_delivery() {
        let session = Uuid::new_v4();
        let connection = Uuid::new_v4();
        let assignment = Uuid::new_v4();
        let status = json!({"desired":"running", "assignment":{"id":assignment,"session_id":session},
            "tenant_id":Uuid::new_v4(),"bot_id":Uuid::new_v4()});
        let mut task = json!({"id":Uuid::new_v4(),"assignment_id":assignment,"session_id":session,
            "connection_id":connection,"delivery_id":Uuid::new_v4(),"status":"completed","editor_deadline":200,
            "forum":{"id":"forum-fixture","principal":"a".repeat(87),"kind":"local"},"result":"private"});
        let (_, projected) = forum_report(&status, &task, session, connection, 100.0).unwrap();
        assert!(projected.get("result").is_none());
        assert!(forum_report(&status, &task, session, connection, 200.0).is_none());
        assert!(forum_report(&status, &task, session, Uuid::new_v4(), 100.0).is_none());
        task["status"] = json!("queued");
        assert!(forum_report(&status, &task, session, connection, 100.0).is_none());
    }

    #[test]
    fn terminal_report_cannot_reuse_a_delivery_after_pause_and_resume() {
        let session = Uuid::new_v4();
        let assignment = Uuid::new_v4();
        let tenant = Uuid::new_v4();
        let bot = Uuid::new_v4();
        let task = Uuid::new_v4();
        let mut gate = automation::Gate::default();
        gate.control(Some(session), automation::Control::Start)
            .unwrap();
        let original = tasks::Consent::capture(&gate, session);
        let report = json!({"taskId":task,"sessionId":session,"assignmentId":assignment,"tenant":tenant,"botId":bot});
        let saved = json!({"identity":{"tenant_id":tenant,"bot_id":bot,"session_id":session,"assignment_id":assignment},
            "result":{"task_id":task},"consent_word":original.word,"resume_seq":original.resume_seq});
        assert!(local_report_consent(&saved, &report)
            .unwrap()
            .matches(&gate));
        gate.control(Some(session), automation::Control::Pause)
            .unwrap();
        gate.control(Some(session), automation::Control::Resume)
            .unwrap();
        assert!(!original.matches(&gate));
        assert!(!local_report_consent(&saved, &report)
            .unwrap()
            .matches(&gate));
        let mut wrong = report;
        wrong["assignmentId"] = json!(Uuid::new_v4());
        assert!(local_report_consent(&saved, &wrong).is_none());
    }

    #[test]
    fn private_configuration_refuses_symlinks_and_shared_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("visor-bot-private-{}", Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("config");
        std::fs::write(&path, b"fixture").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(private_file(&path).unwrap(), b"fixture");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(private_file(&link).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(private_file(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn transport_sends_scoped_headers_and_refuses_redirects() {
        use std::io::Write;
        for response in [
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/leak\r\nContent-Length: 0\r\n\r\n",
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let tenant = Uuid::new_v4();
            let bot = Uuid::new_v4();
            let credential = format!("hm_bot_{}", "a".repeat(64));
            let expected = credential.clone();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                }
                let headers = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
                assert!(headers.contains(&format!("/api/v1/hypermesh/bots/{bot}/agent/heartbeat")));
                assert!(headers.contains(&format!("authorization: bearer {expected}")));
                assert!(headers.contains(&format!("x-tenant-id: {tenant}")));
                let length: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut body = vec![0; length];
                socket.read_exact(&mut body).unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap()["local_consent"],
                    false
                );
                socket.write_all(response.as_bytes()).unwrap();
            });
            let client = Arc::new(Client {
                config_path: PathBuf::new(),
                settings: Settings {
                    origin: format!("http://{addr}"),
                    tenant_id: tenant,
                    bot_id: bot,
                    session_id: Uuid::new_v4(),
                    mode: "interactive".into(),
                    credential_file: PathBuf::new(),
                    forum_prompt: false,
                    forum_reporter_socket: None,
                },
                credential: zeroize::Zeroizing::new(credential),
                connection: Uuid::new_v4(),
                agent: ureq::AgentBuilder::new()
                    .redirects(0)
                    .timeout(Duration::from_secs(2))
                    .build(),
            });
            let result = client
                .post("heartbeat", json!({"local_consent":false}))
                .await;
            assert_eq!(result.is_ok(), response.starts_with("HTTP/1.1 200"));
            if let Err(error) = result {
                assert!(!error.contains("hm_bot_"));
            }
            server.join().unwrap();
        }
    }
    #[test]
    fn session_binding_can_change_after_startup_without_changing_bot_identity() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("visor-bot-rebind-{}", Uuid::new_v4()));
        let settings = Settings {
            origin: "https://example.test".into(),
            tenant_id: Uuid::new_v4(),
            bot_id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            mode: "interactive".into(),
            credential_file: PathBuf::from("/private/token"),
            forum_prompt: false,
            forum_reporter_socket: None,
        };
        let mut updated = serde_json::to_value(&settings).unwrap();
        std::fs::write(&path, updated.to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let client = Client {
            settings,
            config_path: path.clone(),
            credential: zeroize::Zeroizing::new(String::new()),
            connection: Uuid::new_v4(),
            agent: ureq::AgentBuilder::new().build(),
        };
        assert_eq!(client.bound_session().unwrap(), Uuid::nil());
        let created_after_startup = Uuid::new_v4();
        updated["session_id"] = json!(created_after_startup);
        std::fs::write(&path, updated.to_string()).unwrap();
        assert_eq!(client.bound_session().unwrap(), created_after_startup);
        updated["bot_id"] = json!(Uuid::new_v4());
        std::fs::write(&path, updated.to_string()).unwrap();
        assert!(client.bound_session().is_err());
        std::fs::remove_file(path).unwrap();
    }
}
