//! Runs one JavaScript program in the granted GitHub plugin's sandbox.
use super::*;

const MAX_SCRIPT: usize = 8 * 1024;
const MAX_RESULT: usize = 16 * 1024;
const MAX_CALLS: usize = 4;
const MAX_ISSUE: u64 = (1 << 53) - 1;
const RUN_LIMIT: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Script {
    code: String,
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum Frame {
    Read { read: u64 },
    Result { result: Value },
    Failed { failed: bool },
}

#[derive(Clone, Copy, serde::Serialize)]
pub(in crate::api) struct CodeAuditEntry {
    #[serde(skip)]
    session: Uuid,
    run_id: Uuid,
    call: usize,
    tool: &'static str,
    outcome: &'static str,
}

struct AuditCall<'a> {
    state: &'a AppState,
    entry: CodeAuditEntry,
}

impl Drop for AuditCall<'_> {
    fn drop(&mut self) {
        let mut rows = lock(&self.state.github_code_audit);
        if rows.len() == 128 {
            rows.pop_front();
        }
        rows.push_back(self.entry);
    }
}

pub(super) async fn audit(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Path((id, plugin_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    private(link)?;
    let id = parse_id(&id)?;
    if !state.has_session(id) || plugin_id != plugin::GITHUB_ISSUE_PLUGIN_ID {
        return Err(ApiError::missing());
    }
    let rows = lock(&state.github_code_audit);
    let calls: Vec<_> = rows.iter().filter(|row| row.session == id).collect();
    Ok(github::response(StatusCode::OK, json!({"calls": calls})))
}

pub(super) async fn run(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Path((id, plugin_id)): Path<(String, String)>,
    Json(body): Json<Script>,
) -> Result<Response, ApiError> {
    private(link)?;
    let id = parse_id(&id)?;
    if body.code.trim().is_empty() || body.code.len() > MAX_SCRIPT {
        return Err(ApiError::bad(
            "Code Mode accepts 1 to 8192 bytes of JavaScript",
        ));
    }
    let grant = lock(&state.plugins)
        .get(id, &plugin_id)
        .ok_or_else(ApiError::missing)?;
    if !grant.code_mode {
        return Err(ApiError::forbidden(
            "Code Mode requires an opted-in GitHub issue grant",
        ));
    }
    let broker = github::Broker::open(Arc::clone(&state), id, Arc::clone(&grant))?;
    let permit = CALLS
        .try_acquire()
        .map_err(|_| ApiError::unavailable("Plugin workers busy"))?;
    let _permit = permit;
    let deadline = Instant::now() + RUN_LIMIT;
    let run_id = Uuid::new_v4();
    let run = execute(&state, id, run_id, &grant, &broker, &body.code, deadline);
    tokio::pin!(run);
    let mut check = tokio::time::interval(Duration::from_millis(20));
    let timer = tokio::time::sleep_until(deadline.into());
    tokio::pin!(timer);
    loop {
        tokio::select! {
            biased;
            _ = &mut timer => return Ok(failed(plugin::Error::Timeout)),
            _ = check.tick() => if !broker.live() {
                return Ok(failed(plugin::Error::Cancelled));
            },
            result = &mut run => {
                return Ok(match result {
                    Ok((result, calls)) if broker.live() => github::response(
                        StatusCode::OK, json!({"result": result, "run_id": run_id, "calls": calls}),
                    ),
                    Ok(_) => failed(plugin::Error::Cancelled),
                    Err(error) => error,
                });
            }
        }
    }
}

async fn execute(
    state: &AppState,
    session: Uuid,
    run_id: Uuid,
    grant: &plugin::Grant,
    broker: &github::Broker,
    code: &str,
    deadline: Instant,
) -> Result<(Value, usize), Response> {
    let staging = state.mcp_dir.join("plugin-scratch");
    let (mut child, mut channel, _executable) = plugin::stdio::start_code(&grant.package, &staging)
        .await
        .map_err(failed)?;
    channel.send(&json!({"version": 1})).await.map_err(failed)?;
    if channel.receive().await.map_err(failed)? != json!({"version": 1}) {
        return Err(failed(plugin::Error::Incompatible));
    }
    channel.send(&json!({"code": code})).await.map_err(failed)?;
    let mut calls = 0;
    loop {
        let frame = channel.receive().await.map_err(failed)?;
        match serde_json::from_value(frame).map_err(|_| failed(plugin::Error::InvalidOutput))? {
            Frame::Read { read } => {
                calls += 1;
                let mut audit = AuditCall {
                    state,
                    entry: CodeAuditEntry {
                        session,
                        run_id,
                        call: calls,
                        tool: "github.issue.read",
                        outcome: "cancelled",
                    },
                };
                let entry = &mut audit.entry;
                if calls > MAX_CALLS || !(1..=MAX_ISSUE).contains(&read) {
                    entry.outcome = "denied";
                    return Err(github::response(
                        StatusCode::BAD_REQUEST,
                        json!({
                            "error": {"code":"code_mode_read_limit", "message":"Code Mode allows four valid issue reads per run"}
                        }),
                    ));
                }
                let issue = broker.read(read, deadline).await.inspect_err(|error| {
                    entry.outcome = match error.status() {
                        StatusCode::FORBIDDEN => "denied",
                        StatusCode::CONFLICT => "cancelled",
                        _ => "failed",
                    };
                })?;
                entry.outcome = "ok";
                channel.send(&issue).await.map_err(failed)?;
            }
            Frame::Result { result } => {
                if result.to_string().len() > MAX_RESULT {
                    return Err(failed(plugin::Error::InvalidOutput));
                }
                if !child.wait().await.is_ok_and(|status| status.success()) {
                    return Err(failed(plugin::Error::Unavailable));
                }
                return Ok((result, calls));
            }
            Frame::Failed { failed: true } => {
                return Err(github::response(
                    StatusCode::BAD_REQUEST,
                    json!({
                        "error": {"code":"code_mode_script_error", "message":"JavaScript failed or did not return bounded JSON"}
                    }),
                ));
            }
            Frame::Failed { failed: false } => return Err(failed(plugin::Error::InvalidOutput)),
        }
    }
}

fn failed(reason: plugin::Error) -> Response {
    let status = match reason {
        plugin::Error::Timeout => StatusCode::REQUEST_TIMEOUT,
        plugin::Error::Cancelled => StatusCode::CONFLICT,
        plugin::Error::InvalidOutput => StatusCode::BAD_GATEWAY,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    github::response(status, error(reason))
}
