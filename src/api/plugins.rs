//! Plugin control stays on the same private companion transport as workspace reads.
use super::*;
use crate::{local_reads as reads, plugins as plugin};
use serde_json::{json, Value};

static INSTALLS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
static CALLS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/session/:id/plugins", get(list).post(enable))
        .route("/session/:id/plugins/:plugin", delete(disable))
        .route("/session/:id/plugins/:plugin/call", post(call))
}

fn private(link: CompanionLink) -> Result<(), ApiError> {
    if !link.0 {
        return Err(ApiError::forbidden("Local companion socket required"));
    }
    Ok(())
}

fn error(error: plugin::Error) -> Value {
    json!({"error":{"code":error.code(),"message":error.to_string()}})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enable {
    manifest: PathBuf,
    manifest_sha256: String,
    tool: String,
    target: String,
}

async fn enable(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Path(id): Path<String>,
    Json(body): Json<Enable>,
) -> Result<Response, ApiError> {
    private(link)?;
    let id = parse_id(&id)?;
    if !state.has_session(id) {
        return Err(ApiError::missing());
    }
    if body.target.len() > 4096 || !std::path::Path::new(&body.target).is_absolute() {
        return Err(ApiError::bad("An exact workspace file target is required"));
    }
    let permit = INSTALLS
        .try_acquire()
        .map_err(|_| ApiError::unavailable("Plugin installation busy"))?;
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let loaded = plugin::Package::load(&body.manifest, &body.manifest_sha256, &body.tool);
        let package = match loaded {
            Ok(package) => package,
            Err(e) => return Ok((StatusCode::BAD_REQUEST, Json(error(e))).into_response()),
        };
        // Hold session membership through insertion so close cannot miss a grant.
        let sessions = lock(&state.sessions);
        if !sessions.contains_key(&id) {
            return Err(ApiError::missing());
        }
        let metadata = lock(&state.plugins)
            .install(id, package, body.target)
            .ok_or_else(|| ApiError::conflict("Plugin already enabled or grant limit reached"))?;
        Ok((StatusCode::CREATED, Json(metadata)).into_response())
    })
    .await
    .map_err(|_| ApiError::unavailable("Plugin installation unavailable"))?;
    result
}

async fn list(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    private(link)?;
    let id = parse_id(&id)?;
    if !state.has_session(id) {
        return Err(ApiError::missing());
    }
    Ok(Json(json!({"plugins":lock(&state.plugins).list(id)})))
}

async fn disable(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Path((id, plugin)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    private(link)?;
    let id = parse_id(&id)?;
    if !lock(&state.plugins).disable(id, &plugin) {
        return Err(ApiError::missing());
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn call(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Path((id, plugin_id)): Path<(String, String)>,
    Json(body): Json<reads::ReadRequest>,
) -> Result<Response, ApiError> {
    private(link)?;
    let id = parse_id(&id)?;
    let grant = lock(&state.plugins)
        .get(id, &plugin_id)
        .ok_or_else(ApiError::missing)?;
    if body.session_id != id || body.target != grant.target {
        return Err(ApiError::forbidden("Plugin grant does not cover this read"));
    }
    body.selection.validate().map_err(ApiError::bad)?;
    if body.request_id.is_nil()
        || !(1..=plugin::MAX_READ_BYTES).contains(&body.max_bytes)
        || !(1..=10).contains(&body.timeout_seconds)
    {
        return Err(ApiError::bad(
            "Plugin reads allow up to 32 KiB and 10 seconds",
        ));
    }
    let consent = tasks::Consent::capture(&lock(&state.automation), id);
    if !consent.matches(&lock(&state.automation)) {
        return Err(ApiError::forbidden("Plugin requires current local consent"));
    }
    let permit = CALLS
        .try_acquire()
        .map_err(|_| ApiError::unavailable("Plugin workers busy"))?;
    let invocation = grant
        .begin()
        .ok_or_else(|| ApiError::conflict("Plugin busy or disabled"))?;
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        let limit = Duration::from_secs(u64::from(body.timeout_seconds));
        let control = Arc::new(Control {
            grant,
            state,
            consent,
            cancelled: AtomicBool::new(false),
        });
        let _cancel = Cancel(Arc::clone(&control));
        let result = tokio::select! {
            biased;
            _ = tx.closed() => return,
            value = execute(&control, body, limit, permit, invocation) => value,
        };
        let value = match result {
            Ok(result) => json!({"result":result}),
            Err(code) => error(code),
        };
        let _ = tx
            .send(Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                value.to_string(),
            )))
            .await;
    });
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response())
}

struct Control {
    grant: Arc<plugin::Grant>,
    state: Arc<AppState>,
    consent: tasks::Consent,
    cancelled: AtomicBool,
}

impl Control {
    fn live(&self) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && self.grant.live()
            && self.state.has_session(self.consent.session)
            && self.consent.matches(&lock(&self.state.automation))
    }
}

struct Cancel(Arc<Control>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.cancelled.store(true, Ordering::Release);
    }
}

async fn execute(
    control: &Arc<Control>,
    body: reads::ReadRequest,
    limit: Duration,
    permit: tokio::sync::SemaphorePermit<'static>,
    invocation: plugin::Invocation,
) -> Result<Value, plugin::Error> {
    let deadline = Instant::now() + limit;
    let runtime = tokio::runtime::Handle::current();
    let auth = Arc::clone(control);
    let mut review = tokio::task::spawn_blocking(move || {
        let outcome = local_reads::authorized_read(
            &auth.state,
            body,
            || auth.live() && Instant::now() < deadline,
            runtime,
        );
        (outcome, permit, invocation)
    });
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let (read, _permit, _invocation) = loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                if !control.live() { return Err(plugin::Error::Cancelled); }
                if Instant::now() >= deadline { return Err(plugin::Error::Timeout); }
            }
            result = &mut review => break result.map_err(|_| plugin::Error::Unavailable)?,
        }
    };
    let read = read.map_err(|_| plugin::Error::Denied)?;
    let live = || control.live() && reads::now() < read.expires_at;
    if !live() {
        return Err(plugin::Error::Cancelled);
    }
    let staging = control.state.mcp_dir.join("plugin-scratch");
    hypermesh_local_storage::create_dir(&staging).map_err(|_| plugin::Error::Unavailable)?;
    let input =
        json!({"content_base64":base64::engine::general_purpose::STANDARD.encode(&read.bytes)});
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(plugin::Error::Timeout);
    }
    let result = control
        .grant
        .package
        .invoke(&staging, input, remaining, live)
        .await?;
    if !live() {
        return Err(plugin::Error::Cancelled);
    }
    Ok(result)
}
