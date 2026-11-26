use super::{authority, connected_run, declaration, lock, AppState, Authority, Run};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, Method, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use std::{net::Ipv4Addr, num::NonZeroU16, sync::Weak, time::Duration};
use tokio::{net::TcpListener, task::JoinHandle, time::Instant};
use uuid::Uuid;

const LIFETIME: Duration = Duration::from_secs(90);
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;

pub(super) struct Grant {
    pub(super) id: Uuid,
    deadline: Instant,
    url: String,
    task: JoinHandle<()>,
}

impl Grant {
    pub(super) fn open(
        listener: TcpListener,
        state: Weak<AppState>,
        scope: Authority,
        workspace: String,
        run_id: Uuid,
        upstream: NonZeroU16,
    ) -> std::io::Result<Self> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(std::io::Error::other)?;
        let id = Uuid::new_v4();
        let prefix = format!("/{}/", Uuid::new_v4().simple());
        let url = format!("http://{}{}", listener.local_addr()?, prefix);
        let deadline = Instant::now() + LIFETIME;
        let proxy = Proxy {
            state,
            scope,
            workspace,
            run_id,
            id,
            prefix,
            upstream,
            client,
        };
        let task = tokio::spawn(async move {
            let app = Router::new().fallback(fetch).with_state(proxy);
            tokio::select! {
                _ = axum::serve(listener, app) => (),
                _ = tokio::time::sleep_until(deadline) => (),
            }
        });
        Ok(Self {
            id,
            deadline,
            url,
            task,
        })
    }

    pub(super) fn active(&self) -> bool {
        Instant::now() < self.deadline && !self.task.is_finished()
    }

    pub(super) fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) fn run_allowed(state: &AppState, run: &Run, revision: &str) -> bool {
    let gate = lock(&state.automation);
    run.active()
        && !run.cancel
        && run.revision == revision
        && state
            .selected_approved
            .load(std::sync::atomic::Ordering::SeqCst)
        && run
            .preview_consent
            .is_some_and(|grant| grant.matches(&gate))
}

#[derive(Clone)]
struct Proxy {
    state: Weak<AppState>,
    scope: Authority,
    workspace: String,
    run_id: Uuid,
    id: Uuid,
    prefix: String,
    upstream: NonZeroU16,
    client: reqwest::Client,
}

impl Proxy {
    async fn allowed(&self) -> bool {
        let Some(state) = self.state.upgrade() else {
            return false;
        };
        if !matches!(authority(&state, &self.workspace).await, Ok(scope) if scope == self.scope) {
            return false;
        }
        let Ok(Some(task)) = declaration(&state.mcp_dir, &self.workspace) else {
            return false;
        };
        let mut records = lock(&state.terminal_tasks.records);
        let Ok(run) = connected_run(records.get_mut(&self.run_id), &self.scope) else {
            return false;
        };
        run.preview
            .as_ref()
            .is_some_and(|grant| grant.id == self.id && grant.active())
            && task.task.preview_port == Some(self.upstream)
            && run_allowed(&state, run, &task.revision)
    }
}

fn loopback_host(request: &Request) -> bool {
    request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse::<axum::http::uri::Authority>().ok())
        .is_some_and(|host| {
            matches!(host.host(), "localhost" | "127.0.0.1") && host.port_u16().is_some()
        })
}

async fn fetch(State(proxy): State<Proxy>, request: Request) -> Response {
    let method = request.method();
    if !matches!(*method, Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(resource) = request.uri().path().strip_prefix(&proxy.prefix) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !loopback_host(&request) || request.uri().authority().is_some() || resource.starts_with('/')
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !proxy.allowed().await {
        return StatusCode::GONE.into_response();
    }

    let mut target = format!(
        "http://{}:{}/{}",
        Ipv4Addr::LOCALHOST,
        proxy.upstream,
        resource
    );
    if let Some(query) = request.uri().query() {
        target.push('?');
        target.push_str(query);
    }
    // Preserve the declared origin and keep browser credentials and redirects at the proxy.
    let Ok(mut upstream) = proxy.client.request(method.clone(), target).send().await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let status = upstream.status();
    if status.is_redirection()
        || upstream
            .content_length()
            .is_some_and(|n| n > RESPONSE_LIMIT as u64)
    {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let mut body = Vec::new();
    loop {
        match upstream.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= RESPONSE_LIMIT => {
                body.extend_from_slice(&chunk)
            }
            Ok(None) => break,
            _ => return StatusCode::BAD_GATEWAY.into_response(),
        }
    }
    if !proxy.allowed().await {
        return StatusCode::GONE.into_response();
    }
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    if let Some(content_type) = content_type {
        headers.insert(header::CONTENT_TYPE, content_type);
    }
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    headers.insert(header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
            .parse().unwrap());
    response
}
