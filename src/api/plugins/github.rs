//! Owner-only issue reads through the installed plugin's session grant.
use super::*;
use crate::plugins::github::{self as provider, Selection};
use crate::session::Session;

const LIMIT: Duration = Duration::from_secs(8);
static PROVIDER_CALLS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

fn credential(session: &Session) -> Option<zeroize::Zeroizing<Vec<u8>>> {
    let token = session.vault().github_issues_credential()?;
    provider::credential(&token)?;
    Some(token)
}

pub(super) fn has_credential(session: &Session) -> bool {
    credential(session).is_some()
}

pub(super) async fn status(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Path((id, plugin_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    private(link)?;
    let id = parse_id(&id)?;
    if !state.has_session(id) {
        return Err(ApiError::missing());
    }
    Ok(publication_task(move || {
        with_store(&state, |store| {
            let entry = store.get(&plugin_id)?.ok_or(PublicationError::Missing)?;
            let grant = lock(&state.plugins).get(id, &plugin_id);
            let access = grant.as_ref().and_then(|grant| match &grant.target {
                plugin::Target::GitHub(selection) => Some(selection),
                plugin::Target::Workspace(_) => None,
            });
            let sessions = lock(&state.sessions);
            let Some(session) = sessions.get(&id) else {
                return Ok(ApiError::missing().into_response());
            };
            let consent = tasks::Consent::capture(&lock(&state.automation), id);
            let status = if !entry.enabled || access.is_none() {
                "revoked"
            } else if !has_credential(session) || !consent.matches(&lock(&state.automation)) {
                "unavailable"
            } else {
                "active"
            };
            let mut summary = json!({
                "id":plugin_id,
                "package_version":entry.metadata.package_version,
                "publisher":entry.metadata.publisher,
                "state":status,
                "access":access.filter(|_| status == "active")
                    .map(|selection| json!({"scope":"github.issue.read","target":selection}))
            });
            if grant.as_ref().is_some_and(|grant| grant.code_mode) {
                summary["code_mode"] = json!(status == "active");
            }
            Ok(Json(summary).into_response())
        })
    })
    .await)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Read {
    target: Selection,
    number: u64,
}

pub(super) async fn read(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Path((id, plugin_id)): Path<(String, String)>,
    Json(body): Json<Read>,
) -> Result<Response, ApiError> {
    private(link)?;
    let id = parse_id(&id)?;
    let grant = lock(&state.plugins)
        .get(id, &plugin_id)
        .ok_or_else(ApiError::missing)?;
    let plugin::Target::GitHub(selected) = &grant.target else {
        return Err(ApiError::forbidden(
            "Plugin grant does not cover GitHub issue reads",
        ));
    };
    if selected != &body.target || body.number == 0 {
        return Err(ApiError::forbidden(
            "Plugin grant does not cover this account and repository",
        ));
    }
    let broker = Broker::open(state, id, grant)?;
    Ok(
        match broker.read(body.number, Instant::now() + LIMIT).await {
            Ok(issue) => response(StatusCode::OK, json!({"result": issue})),
            Err(error) => error,
        },
    )
}

pub(super) struct Broker {
    control: Arc<Control>,
    selected: Selection,
    invocation: Arc<plugin::Invocation>,
    _cancel: Cancel,
}

impl Broker {
    pub(super) fn open(
        state: Arc<AppState>,
        id: Uuid,
        grant: Arc<plugin::Grant>,
    ) -> Result<Self, ApiError> {
        let plugin::Target::GitHub(selected) = &grant.target else {
            return Err(ApiError::forbidden(
                "Plugin grant does not cover GitHub issue reads",
            ));
        };
        let selected = selected.clone();
        let consent = tasks::Consent::capture(&lock(&state.automation), id);
        if !consent.matches(&lock(&state.automation)) {
            return Err(ApiError::forbidden("Plugin requires current local consent"));
        }
        let invocation = grant
            .begin()
            .ok_or_else(|| ApiError::conflict("Plugin busy or disabled"))?;
        let control = Arc::new(Control {
            grant,
            state,
            consent,
            cancelled: AtomicBool::new(false),
        });
        Ok(Self {
            _cancel: Cancel(Arc::clone(&control)),
            control,
            selected,
            invocation: Arc::new(invocation),
        })
    }

    pub(super) fn live(&self) -> bool {
        self.control.live()
    }

    pub(super) async fn read(&self, number: u64, run_deadline: Instant) -> Result<Value, Response> {
        if !self.live() {
            return Err(failure(provider::Error::Cancelled));
        }
        let deadline = run_deadline.min(Instant::now() + LIMIT);
        if Instant::now() >= deadline {
            return Err(failure(provider::Error::Unavailable));
        }
        let control = &self.control;
        let id = control.consent.session;
        let token = lock(&control.state.sessions)
            .get(&id)
            .and_then(credential)
            .ok_or_else(|| {
                ApiError::forbidden("GitHub issue credential unavailable").into_response()
            })?;
        let permit = PROVIDER_CALLS
            .try_acquire()
            .map_err(|_| ApiError::unavailable("GitHub issue workers busy").into_response())?;
        let worker = Arc::clone(control);
        let invocation = Arc::clone(&self.invocation);
        let selected = self.selected.clone();
        let mut fetch = tokio::task::spawn_blocking(move || {
            let live = || Instant::now() < deadline && worker.live();
            let read = || provider::read(&selected, &selected, number, &token, live);
            #[cfg(test)]
            let result = match worker.state.github_fixture {
                Some(address) => {
                    provider::fixture_read(address, &selected, &selected, number, &token, live)
                }
                None => read(),
            };
            #[cfg(not(test))]
            let result = read();
            (result, permit, invocation)
        });
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let (result, _permit, _invocation) = loop {
            tokio::select! {
                biased;
                _ = tick.tick() => {
                    if !control.live() {
                        return Err(failure(provider::Error::Cancelled));
                    }
                    if Instant::now() >= deadline {
                        return Err(failure(provider::Error::Unavailable));
                    }
                }
                result = &mut fetch => break result.map_err(|_| {
                    ApiError::unavailable("GitHub issue read unavailable").into_response()
                })?,
            }
        };
        let issue = match result {
            Ok(issue) => issue,
            Err(failed) => {
                if failed == provider::Error::Revoked {
                    let mut grants = lock(&control.state.plugins);
                    let plugin_id = control.grant.package.key();
                    if grants
                        .get(id, plugin_id)
                        .is_some_and(|grant| Arc::ptr_eq(&grant, &control.grant))
                    {
                        grants.disable(id, plugin_id);
                    }
                }
                return Err(failure(failed));
            }
        };
        let staging = control.state.mcp_dir.join("plugin-scratch");
        let result = run_package(control, &staging, json!({"issue":issue}), deadline, || {
            control.live()
        })
        .await;
        match result {
            Ok(result) if result == issue => Ok(result),
            Ok(_) => Err(response(
                StatusCode::BAD_GATEWAY,
                error(plugin::Error::InvalidOutput),
            )),
            Err(failed) => Err(response(StatusCode::SERVICE_UNAVAILABLE, error(failed))),
        }
    }
}

pub(super) fn response(status: StatusCode, body: Value) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn failure(error: provider::Error) -> Response {
    let (status, code, message) = match error {
        provider::Error::Refused => (
            StatusCode::FORBIDDEN,
            "github_issue_refused",
            "GitHub issue read refused",
        ),
        provider::Error::Revoked => (
            StatusCode::FORBIDDEN,
            "github_credential_revoked",
            "GitHub credential revoked",
        ),
        provider::Error::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "github_unavailable",
            "GitHub issue service unavailable",
        ),
        provider::Error::Cancelled => (
            StatusCode::CONFLICT,
            "plugin_cancelled",
            "GitHub issue grant ended",
        ),
    };
    response(status, json!({"error":{"code":code,"message":message}}))
}
