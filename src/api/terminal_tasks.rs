//! Local, operator-declared terminal tasks on the private companion connection.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{io::Read, process::Stdio};
use tokio::io::AsyncReadExt;

const OUTPUT_LIMIT: usize = 128 * 1024;
const PAGE_SIZE: usize = 16 * 1024;
const MAX_RUNS: usize = 16;
const TICK: Duration = Duration::from_millis(100);

pub(super) struct Runs {
    boot: Uuid,
    records: Mutex<HashMap<Uuid, Run>>,
}

impl Runs {
    pub(super) fn new() -> Self {
        Self {
            boot: Uuid::new_v4(),
            records: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn close(&self, session: Uuid) {
        let mut records = lock(&self.records);
        records.retain(|_, run| {
            if run.session == session {
                run.cancel = true;
                return run.active();
            }
            true
        });
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    workspace: PathBuf,
    cwd: PathBuf,
    command: PathBuf,
    args: Vec<String>,
    timeout_seconds: u64,
}

struct Declaration {
    task: Task,
    revision: String,
}

fn declaration(dir: &std::path::Path, workspace: &str) -> Result<Option<Declaration>, ()> {
    use hypermesh_local_storage::{open, Open};
    let path = dir.join("terminal-task.json");
    let file = match open(&path, Open::Read { private: true }) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes).map_err(|_| ())?;
    if bytes.len() > 8192 {
        return Err(());
    }
    let task: Task = serde_json::from_slice(&bytes).map_err(|_| ())?;
    // Only the assigned, canonical local workspace and a directory within it
    // may be used. Neither client supplies a command or environment.
    if task.workspace != std::path::Path::new(workspace)
        || task.workspace.canonicalize().map_err(|_| ())? != task.workspace
        || task.cwd.canonicalize().map_err(|_| ())? != task.cwd
        || !task.cwd.starts_with(&task.workspace)
        || !task.cwd.is_dir()
        || !task.command.is_absolute()
        || !task.command.is_file()
        || task
            .command
            .to_str()
            .is_none_or(|text| text.chars().any(char::is_control))
        || task.args.len() > 16
        || task
            .args
            .iter()
            .any(|arg| arg.len() > 512 || arg.chars().any(char::is_control))
        || !(1..=300).contains(&task.timeout_seconds)
    {
        return Err(());
    }
    Ok(Some(Declaration {
        task,
        revision: format!("{:x}", Sha256::digest(bytes)),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RequestBody {
    workspace: String,
    operation: Operation,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Context,
    Start {
        boot: Uuid,
        session: Uuid,
        binding: String,
        id: Uuid,
        revision: String,
    },
    Read {
        boot: Uuid,
        session: Uuid,
        binding: String,
        id: Uuid,
        cursor: usize,
    },
    Cancel {
        boot: Uuid,
        session: Uuid,
        binding: String,
        id: Uuid,
    },
}

#[derive(Clone, PartialEq, Eq)]
struct Authority {
    session: Uuid,
    binding: String,
}

async fn authority(state: &AppState, workspace: &str) -> Result<Authority, ApiError> {
    if state.managed_guest.is_some()
        || !std::path::Path::new(workspace).is_absolute()
        || workspace.len() > 4096
        || workspace.chars().any(char::is_control)
    {
        return Err(ApiError::forbidden("Assigned local workspace required"));
    }
    let bridge = state
        .host_bridge
        .as_ref()
        .ok_or_else(|| ApiError::unavailable("Assigned local workspace unavailable"))?;
    let value = tokio::time::timeout(Duration::from_secs(2), bridge.machine_context())
        .await
        .map_err(|_| ApiError::unavailable("Assigned local workspace unavailable"))?
        .map_err(|_| ApiError::unavailable("Assigned local workspace unavailable"))?;
    let session = acp_machine::binding(&value, workspace)
        .map_err(|_| ApiError::forbidden("Assigned local workspace required"))?;
    if !state.has_session(session)
        || *lock(&state.selected_session) != Some(session)
        || !state.selected_approved.load(Ordering::SeqCst)
    {
        return Err(ApiError::forbidden(
            "Current approved local session required",
        ));
    }
    let identity = acp_machine::context_view(&value, session);
    Ok(Authority {
        session,
        binding: format!("{:x}", Sha256::digest(identity.to_string())),
    })
}

struct Run {
    session: Uuid,
    binding: String,
    revision: String,
    status: &'static str,
    exit_code: Option<i32>,
    output: Vec<u8>,
    truncated: bool,
    cancel: bool,
}

impl Run {
    fn active(&self) -> bool {
        self.status == "running"
    }

    fn page(&self, id: Uuid, cursor: usize, allowed: bool) -> Result<Value, ApiError> {
        let end = self.output.len().min(cursor.saturating_add(PAGE_SIZE));
        let output = self
            .output
            .get(cursor..end)
            .ok_or_else(|| ApiError::bad("Invalid output cursor"))?;
        Ok(json!({
            "id": id, "session": self.session, "status": self.status, "exit_code": self.exit_code,
            "access": if allowed { "ready" } else { "paused" },
            "cursor": end, "more": end < self.output.len(), "truncated": self.truncated,
            "output_base64": STANDARD.encode(output),
        }))
    }
}

fn require_binding(
    store: &Runs,
    authority: &Authority,
    boot: Uuid,
    session: Uuid,
    binding: &str,
) -> Result<(), ApiError> {
    if store.boot != boot || authority.session != session || authority.binding != binding {
        return Err(ApiError::forbidden("Terminal task assignment changed"));
    }
    Ok(())
}

fn local_consent(state: &AppState, session: Uuid) -> Option<tasks::Consent> {
    let gate = lock(&state.automation);
    let consent = tasks::Consent::capture(&gate, session);
    consent.matches(&gate).then_some(consent)
}

fn connected_run<'a>(
    run: Option<&'a mut Run>,
    binding: &Authority,
) -> Result<&'a mut Run, ApiError> {
    run.filter(|run| run.session == binding.session && run.binding == binding.binding)
        .ok_or_else(ApiError::missing)
}

pub(super) async fn exchange(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Json(body): Json<RequestBody>,
) -> Result<Json<Value>, ApiError> {
    if !link.0 {
        return Err(ApiError::forbidden("Private companion connection required"));
    }
    let authority = authority(&state, &body.workspace).await?;
    let store = &state.terminal_tasks;
    let consent = local_consent(&state, authority.session);
    match body.operation {
        Operation::Context => {
            let declared = declaration(&state.mcp_dir, &body.workspace)
                .map_err(|_| ApiError::forbidden("Terminal task declaration refused"))?;
            let task = declared.map(|d| {
                json!({
                    "revision": d.revision, "cwd": d.task.cwd, "command": d.task.command,
                    "args": d.task.args, "timeout_seconds": d.task.timeout_seconds,
                    "environment": "clean",
                })
            });
            Ok(Json(json!({
                "boot": store.boot, "workspace": body.workspace, "session": authority.session,
                "binding": authority.binding, "task": task,
                "access": if consent.is_some() { "ready" } else { "paused" },
            })))
        }
        Operation::Start {
            boot,
            session,
            binding,
            id,
            revision,
        } => {
            require_binding(store, &authority, boot, session, &binding)?;
            if id.is_nil() || revision.len() != 64 {
                return Err(ApiError::bad(
                    "Invalid terminal task identifier or declaration",
                ));
            }
            let mut records = lock(&store.records);
            if records.contains_key(&id) {
                let run = connected_run(records.get_mut(&id), &authority)?;
                if run.revision != revision {
                    return Err(ApiError::conflict("Task ID already used"));
                }
                return Ok(Json(run.page(id, 0, consent.is_some())?));
            }
            if records.len() >= MAX_RUNS || records.values().any(Run::active) {
                return Err(ApiError::conflict("Terminal task capacity reached"));
            }
            let consent =
                consent.ok_or_else(|| ApiError::forbidden("Local terminal access is paused"))?;
            let declared = declaration(&state.mcp_dir, &body.workspace)
                .map_err(|_| ApiError::forbidden("Terminal task declaration refused"))?
                .filter(|task| task.revision == revision)
                .ok_or_else(|| {
                    ApiError::conflict("Review the current terminal task declaration")
                })?;
            let (child, group) = {
                let gate = lock(&state.automation);
                if !consent.matches(&gate) || !state.selected_approved.load(Ordering::SeqCst) {
                    return Err(ApiError::forbidden("Local terminal access is paused"));
                }
                spawn(&declared.task)?
            };
            let run = Run {
                session,
                binding,
                revision,
                status: "running",
                exit_code: None,
                output: Vec::new(),
                truncated: false,
                cancel: false,
            };
            let response = run.page(id, 0, true)?;
            records.insert(id, run);
            drop(records);
            tokio::spawn(supervise(
                state.clone(),
                id,
                body.workspace,
                authority,
                consent,
                declared,
                child,
                group,
            ));
            Ok(Json(response))
        }
        Operation::Read {
            boot,
            session,
            binding,
            id,
            cursor,
        } => {
            require_binding(store, &authority, boot, session, &binding)?;
            let mut records = lock(&store.records);
            Ok(Json(
                connected_run(records.get_mut(&id), &authority)?.page(
                    id,
                    cursor,
                    consent.is_some(),
                )?,
            ))
        }
        Operation::Cancel {
            boot,
            session,
            binding,
            id,
        } => {
            require_binding(store, &authority, boot, session, &binding)?;
            let mut records = lock(&store.records);
            let run = connected_run(records.get_mut(&id), &authority)?;
            run.cancel = true;
            Ok(Json(run.page(id, 0, consent.is_some())?))
        }
    }
}

fn spawn(task: &Task) -> Result<(tokio::process::Child, ProcessGroup), ApiError> {
    let mut command = tokio::process::Command::new(&task.command);
    command
        .args(&task.args)
        .current_dir(&task.cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let child = command
        .spawn()
        .map_err(|_| ApiError::unavailable("Could not start the declared terminal task"))?;
    let group = ProcessGroup(child.id().expect("spawned child has an ID") as i32);
    Ok((child, group))
}

struct ProcessGroup(i32);
impl ProcessGroup {
    fn leader_exited(&self) -> std::io::Result<bool> {
        // Keep the leader's PID reserved until the group has been stopped.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.0 as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { info.si_pid() } == self.0)
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // The child is the leader of the dedicated group.
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}

async fn wait_for_revocation(
    state: &AppState,
    workspace: &str,
    expected: &Authority,
    revision: &str,
) {
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if !matches!(authority(state, workspace).await, Ok(current) if current == *expected)
            || !matches!(declaration(&state.mcp_dir, workspace), Ok(Some(task)) if task.revision == revision)
        {
            return;
        }
    }
}

fn append_output(run: &mut Run, bytes: &[u8]) {
    let count = bytes.len().min(OUTPUT_LIMIT - run.output.len());
    run.output.extend_from_slice(&bytes[..count]);
    run.truncated |= count < bytes.len();
}

async fn supervise(
    state: Arc<AppState>,
    id: Uuid,
    workspace: String,
    scope: Authority,
    consent: tasks::Consent,
    declared: Declaration,
    mut child: tokio::process::Child,
    group: ProcessGroup,
) {
    let mut stdout = child.stdout.take().expect("configured stdout");
    let mut stderr = child.stderr.take().expect("configured stderr");
    let mut out = [0; 4096];
    let mut err = [0; 4096];
    let (mut out_open, mut err_open) = (true, true);
    let mut interval = tokio::time::interval(TICK);
    let deadline = tokio::time::sleep(Duration::from_secs(declared.task.timeout_seconds));
    let revoke = wait_for_revocation(&state, &workspace, &scope, &declared.revision);
    tokio::pin!(deadline, revoke);
    let mut status = loop {
        tokio::select! {
            biased;
            _ = &mut deadline => break "timed_out",
            _ = &mut revoke => break "revoked",
            _ = interval.tick() => {
                if !state.has_session(scope.session) || !state.selected_approved.load(Ordering::SeqCst) {
                    break "revoked";
                }
                if !consent.matches(&lock(&state.automation)) {
                    break "paused";
                }
                if lock(&state.terminal_tasks.records).get(&id).is_none_or(|run| run.cancel) {
                    break "cancelled";
                }
                if !out_open && !err_open {
                    match group.leader_exited() {
                        Ok(true) => break "exited",
                        Ok(false) => {}
                        Err(_) => break "failed",
                    }
                }
            }
            read = stdout.read(&mut out), if out_open => match read {
                Ok(0) => out_open = false,
                Ok(count) => if let Some(run) = lock(&state.terminal_tasks.records).get_mut(&id) {
                    append_output(run, &out[..count]);
                },
                Err(_) => break "failed",
            },
            read = stderr.read(&mut err), if err_open => match read {
                Ok(0) => err_open = false,
                Ok(count) => if let Some(run) = lock(&state.terminal_tasks.records).get_mut(&id) {
                    append_output(run, &err[..count]);
                },
                Err(_) => break "failed",
            },
        }
    };
    drop(group);
    let exit = child.wait().await.ok();
    let mut code = None;
    if status == "exited" {
        code = exit.as_ref().and_then(std::process::ExitStatus::code);
        status = if exit.is_some_and(|exit| exit.success()) {
            "completed"
        } else {
            "failed"
        };
    }
    let mut records = lock(&state.terminal_tasks.records);
    if !state.has_session(scope.session) {
        records.remove(&id);
    } else if let Some(run) = records.get_mut(&id) {
        if status == "revoked" {
            run.output.clear();
        }
        run.status = status;
        run.exit_code = code;
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::api::tests::{task_fixture, trust_test_request};
    use std::io::Write;

    async fn call(
        app: &Router,
        workspace: &std::path::Path,
        operation: Value,
    ) -> (StatusCode, Value) {
        trust_test_request(
            app.clone(),
            "POST",
            "/terminal-task",
            json!({"workspace":workspace,"operation":operation}),
        )
        .await
    }

    #[tokio::test]
    async fn declared_workspace_task_is_shared_and_revoked_with_its_session() {
        use http_body_util::Full;
        use hyper_util::rt::TokioIo;
        use std::fs;

        let (mut state, _, session) = task_fixture();
        let directory =
            std::env::temp_dir().join(format!("visor-terminal-{}", Uuid::new_v4().simple()));
        hypermesh_local_storage::create_dir(&directory).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        let workspace = directory.join("workspace");
        let cwd = workspace.join("subdir");
        fs::create_dir_all(&cwd).unwrap();
        let socket = directory.join("host.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let identity = json!({
            "workspace":workspace, "identity":{
                "session_id":session,"tenant_id":Uuid::new_v4(),"device_id":Uuid::new_v4(),
                "bot_id":Uuid::new_v4(),"assignment_id":Uuid::new_v4(),
                "organization_id":Uuid::new_v4(),"owner_id":Uuid::new_v4(),
            },
        });
        let fixture = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let response = identity.to_string();
                tokio::spawn(async move {
                    let service =
                        hyper::service::service_fn(|req: hyper::Request<hyper::body::Incoming>| {
                            assert_eq!(req.uri().path(), "/machine-context");
                            let response = response.clone();
                            async {
                                Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::new(
                                    Bytes::from(response),
                                )))
                            }
                        });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        let private = Arc::get_mut(&mut state).unwrap();
        private.host_bridge = Some(crate::host_bridge::Bridge::plugin_contract(socket));
        private.mcp_dir = directory.clone();
        assert!(state.select_companion_session(session).is_ok());
        let epoch = lock(&state.automation).epoch();
        assert!(state
            .control_automation_at(Control::Start, true, Some(session), Some(epoch))
            .is_ok());
        let config = directory.join("terminal-task.json");
        let write_task = |command: &str, args: &[&str]| {
            if config.exists() {
                fs::remove_file(&config).unwrap();
            }
            let mut output =
                hypermesh_local_storage::open(&config, hypermesh_local_storage::Open::New).unwrap();
            writeln!(
                output,
                "{}",
                json!({"workspace":workspace,"cwd":cwd,
                "command":command,"args":args,"timeout_seconds":5})
            )
            .unwrap();
        };
        write_task("/bin/sh", &["-c", "pwd; printf x >> launches"]);
        let app = companion_router(state.clone());
        let network = router(state.clone());
        assert_eq!(
            call(&network, &workspace, json!({"type":"context"}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&app, &directory, json!({"type":"context"})).await.0,
            StatusCode::FORBIDDEN
        );
        let (status, scope) = call(&app, &workspace, json!({"type":"context"})).await;
        assert_eq!(status, StatusCode::OK, "{scope}");
        assert_eq!(scope["task"]["cwd"], json!(cwd));

        let id = Uuid::new_v4();
        let base = |kind: &str, id: Uuid| json!({"type":kind,"boot":scope["boot"],"session":session,"binding":scope["binding"],"id":id});
        let mut start = base("start", id);
        start["revision"] = scope["task"]["revision"].clone();
        let (status, cli) = call(&app, &workspace, start.clone()).await;
        assert_eq!(status, StatusCode::OK, "{cli}");
        let (status, editor) = call(&app, &workspace, start).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cli["id"], editor["id"]);

        let mut read = base("read", id);
        read["cursor"] = json!(0);
        let finished = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (status, page) = call(&app, &workspace, read.clone()).await;
                assert_eq!(status, StatusCode::OK, "{page}");
                if page["status"] != "running" {
                    break page;
                }
                tokio::time::sleep(TICK).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(finished["status"], "completed");
        assert_eq!(finished["exit_code"], 0);
        let bytes = STANDARD
            .decode(finished["output_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap().trim(),
            cwd.to_str().unwrap()
        );
        assert_eq!(fs::read(cwd.join("launches")).unwrap(), b"x");
        read["cursor"] = finished["cursor"].clone();
        let (_, attached) = call(&app, &workspace, read.clone()).await;
        assert_eq!(attached["id"], finished["id"]);
        assert_eq!(attached["exit_code"], finished["exit_code"]);
        assert_eq!(attached["output_base64"], "");

        write_task("/bin/sleep", &["10"]);
        let (_, updated) = call(&app, &workspace, json!({"type":"context"})).await;
        let active = Uuid::new_v4();
        let mut start = base("start", active);
        start["revision"] = updated["task"]["revision"].clone();
        assert_eq!(call(&app, &workspace, start).await.0, StatusCode::OK);
        assert_eq!(
            call(&app, &workspace, base("cancel", active)).await.0,
            StatusCode::OK
        );
        let mut cancelled = base("read", active);
        cancelled["cursor"] = json!(0);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (_, page) = call(&app, &workspace, cancelled.clone()).await;
                if page["status"] == "cancelled" {
                    break;
                }
                tokio::time::sleep(TICK).await;
            }
        })
        .await
        .unwrap();
        assert!(state.control_automation(Control::Pause, true, None).is_ok());
        let (_, paused) = call(&app, &workspace, read.clone()).await;
        assert_eq!(paused["access"], "paused");
        let mut refused = base("start", Uuid::new_v4());
        refused["revision"] = updated["task"]["revision"].clone();
        assert_eq!(
            call(&app, &workspace, refused).await.0,
            StatusCode::FORBIDDEN
        );
        lock(&state.sessions).remove(&session);
        assert_eq!(call(&app, &workspace, read).await.0, StatusCode::FORBIDDEN);
        fixture.abort();
    }
}
