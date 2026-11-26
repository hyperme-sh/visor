//! Plugin control stays on the same private companion transport as workspace reads.
mod github;
mod github_code;
pub(super) use github_code::CodeAuditEntry;

use super::*;
use crate::{local_reads as reads, plugins as plugin};
use plugin::{
    publication::{Error as PublicationError, Publication},
    store::Store,
};
use serde_json::{json, Value};

static INSTALLS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
static CALLS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/plugins", get(installed_list).post(install))
        .route("/plugins/inspect", post(inspect))
        .route("/plugins/:plugin", delete(remove).put(set_enabled))
        .route("/session/:id/plugins", get(list).post(enable))
        .route("/session/:id/plugins/:plugin", delete(disable))
        .route("/session/:id/plugins/:plugin/call", post(call))
        .route(
            "/session/:id/plugins/:plugin/github-issue",
            get(github::status).post(github::read),
        )
        .route(
            "/session/:id/plugins/:plugin/github-issue/code",
            get(github_code::audit)
                .post(github_code::run)
                .layer(axum::extract::DefaultBodyLimit::max(64 * 1024)),
        )
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

fn publication_error(error: PublicationError) -> Response {
    let status = match error {
        PublicationError::Missing => StatusCode::NOT_FOUND,
        PublicationError::Disabled | PublicationError::Changed | PublicationError::Bundled => {
            StatusCode::CONFLICT
        }
        PublicationError::Publisher => StatusCode::FORBIDDEN,
        PublicationError::Storage => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_REQUEST,
    };
    (
        status,
        Json(json!({"error":{"code":error.code(),"message":error.message()}})),
    )
        .into_response()
}

fn with_store<T>(
    state: &AppState,
    run: impl FnOnce(&Store) -> Result<T, PublicationError>,
) -> Result<T, PublicationError> {
    let mut store = lock(&state.plugin_store);
    if store.is_none() {
        *store = Some(Store::open(&state.mcp_dir)?);
    }
    run(store.as_ref().expect("store was initialized"))
}

async fn publication_task(
    work: impl FnOnce() -> Result<Response, PublicationError> + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => publication_error(error),
        Err(_) => publication_error(PublicationError::Storage),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inspect {
    source: String,
}

async fn inspect(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Json(body): Json<Inspect>,
) -> Result<Response, ApiError> {
    private(link)?;
    let permit = INSTALLS
        .try_acquire()
        .map_err(|_| ApiError::unavailable("Plugin installer busy"))?;
    Ok(publication_task(move || {
        let _permit = permit;
        let package = Publication::read(&body.source, None)?;
        with_store(&state, |store| {
            let before = store.get(&package.metadata.id)?;
            let added_access = package
                .metadata
                .added_access(before.as_ref().map(|entry| &entry.metadata));
            Ok(Json(json!({
                "publication":package.metadata,
                "installed":before,
                "added_or_changed_tools":added_access
            }))
            .into_response())
        })
    })
    .await)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Install {
    source: String,
    publication_sha256: String,
    expected_revision: Option<Uuid>,
}

async fn install(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Json(body): Json<Install>,
) -> Result<Response, ApiError> {
    private(link)?;
    let permit = INSTALLS
        .try_acquire()
        .map_err(|_| ApiError::unavailable("Plugin installer busy"))?;
    Ok(publication_task(move || {
        let _permit = permit;
        let package = Publication::read(&body.source, Some(&body.publication_sha256))?;
        let id = package.metadata.id.clone();
        with_store(&state, |store| {
            let entry = store.install(package, body.expected_revision, || {
                lock(&state.plugins).revoke_plugin(&id)
            })?;
            let status = if body.expected_revision.is_some() {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            Ok((status, Json(entry)).into_response())
        })
    })
    .await)
}

async fn installed_list(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
) -> Result<Response, ApiError> {
    private(link)?;
    Ok(publication_task(move || {
        with_store(&state, |store| {
            Ok(Json(json!({"plugins":store.list()?})).into_response())
        })
    })
    .await)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enabled {
    expected_revision: Uuid,
    enabled: bool,
}

async fn set_enabled(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Path(id): Path<String>,
    Json(body): Json<Enabled>,
) -> Result<Response, ApiError> {
    private(link)?;
    Ok(publication_task(move || {
        with_store(&state, |store| {
            let entry = store.set_enabled(&id, body.expected_revision, body.enabled, || {
                lock(&state.plugins).revoke_plugin(&id)
            })?;
            Ok(Json(entry).into_response())
        })
    })
    .await)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Remove {
    expected_revision: Uuid,
}

async fn remove(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Path(id): Path<String>,
    Json(body): Json<Remove>,
) -> Result<Response, ApiError> {
    private(link)?;
    Ok(publication_task(move || {
        with_store(&state, |store| {
            store.remove(&id, body.expected_revision, || {
                lock(&state.plugins).revoke_plugin(&id)
            })?;
            Ok(StatusCode::NO_CONTENT.into_response())
        })
    })
    .await)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Enable {
    Local(LocalEnable),
    Installed(InstalledEnable),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalEnable {
    manifest: PathBuf,
    manifest_sha256: String,
    tool: String,
    target: plugin::Target,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstalledEnable {
    plugin_id: String,
    revision: Uuid,
    tool: String,
    target: plugin::Target,
    #[serde(default)]
    code_mode: bool,
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
    let permit = INSTALLS
        .try_acquire()
        .map_err(|_| ApiError::unavailable("Plugin installer busy"))?;
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        with_store(&state, |store| {
            let (package, target, publisher, code_mode) = match body {
                Enable::Local(body) => {
                    if !matches!(body.target, plugin::Target::Workspace(_)) {
                        return Ok(ApiError::bad(
                            "Install this plugin before enabling GitHub access",
                        )
                        .into_response());
                    }
                    let package =
                        plugin::Package::load(&body.manifest, &body.manifest_sha256, &body.tool)?;
                    if store.get(package.key())?.is_some() {
                        return Err(PublicationError::Changed);
                    }
                    (package, body.target, None, false)
                }
                Enable::Installed(body) => (
                    store.activate(&body.plugin_id, body.revision, &body.tool)?,
                    body.target,
                    store
                        .get(&body.plugin_id)?
                        .map(|entry| entry.metadata.publisher),
                    body.code_mode,
                ),
            };
            if !package.allows(&target) {
                return Ok(ApiError::bad("Plugin scope does not cover this target").into_response());
            }
            if code_mode
                && (package.key() != plugin::GITHUB_ISSUE_PLUGIN_ID
                    || !matches!(target, plugin::Target::GitHub(_)))
            {
                return Ok(
                    ApiError::bad("Code Mode requires the GitHub issue pilot").into_response()
                );
            }
            // Keep membership and the installed revision stable until insertion.
            let sessions = lock(&state.sessions);
            let Some(session) = sessions.get(&id) else {
                return Ok(ApiError::missing().into_response());
            };
            let access = if let plugin::Target::GitHub(selection) = &target {
                if !github::has_credential(session) {
                    return Ok(
                        ApiError::forbidden("GitHub issue credential unavailable").into_response()
                    );
                }
                Some(json!({"scope":"github.issue.read","target":selection}))
            } else {
                None
            };
            let mut metadata = lock(&state.plugins).install(id, package, target, code_mode);
            if let (Some(metadata), Some(access)) = (&mut metadata, access) {
                metadata["publisher"] = json!(publisher);
                metadata["access"] = access;
            }
            Ok(match metadata {
                Some(metadata) => (StatusCode::CREATED, Json(metadata)).into_response(),
                None => ApiError::conflict("Plugin already enabled or grant limit reached")
                    .into_response(),
            })
        })
    })
    .await
    .map_err(|_| ApiError::unavailable("Plugin activation unavailable"))?;
    Ok(result.unwrap_or_else(publication_error))
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
    if body.session_id != id
        || !matches!(&grant.target, plugin::Target::Workspace(path) if *path == body.target)
    {
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
    let input =
        json!({"content_base64":base64::engine::general_purpose::STANDARD.encode(&read.bytes)});
    run_package(control, &staging, input, deadline, live).await
}

async fn run_package(
    control: &Control,
    staging: &std::path::Path,
    input: Value,
    deadline: Instant,
    live: impl Fn() -> bool,
) -> Result<Value, plugin::Error> {
    hypermesh_local_storage::create_dir(staging).map_err(|_| plugin::Error::Unavailable)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(plugin::Error::Timeout);
    }
    let result = control
        .grant
        .package
        .invoke(staging, input, remaining, &live)
        .await?;
    if !live() {
        return Err(plugin::Error::Cancelled);
    }
    Ok(result)
}
