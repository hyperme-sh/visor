//! Local, operator-declared terminal tasks on the private companion connection.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{io::Read, process::Stdio};
use tokio::io::AsyncReadExt;

mod interactive;
mod process_group;
use interactive::{ControlRequest, Master, Terminal};
use process_group::ProcessGroup;

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
    #[serde(default)]
    interactive: bool,
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
    wsl_distro: Option<String>,
    expected_device_id: Option<Uuid>,
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
    Control {
        boot: Uuid,
        session: Uuid,
        binding: String,
        id: Uuid,
        terminal: Uuid,
        request: ControlRequest,
    },
}

#[derive(Clone, PartialEq, Eq)]
struct Authority {
    session: Uuid,
    device_id: Uuid,
    binding: String,
}

pub(super) async fn assigned_context(
    state: &AppState,
    workspace: &str,
) -> Result<(Uuid, Value), ApiError> {
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
    Ok((session, value))
}

async fn authority(state: &AppState, workspace: &str) -> Result<Authority, ApiError> {
    let (session, value) = assigned_context(state, workspace).await?;
    let identity = acp_machine::context_view(&value, session);
    let device_id = serde_json::from_value(value["identity"]["device_id"].clone())
        .map_err(|_| ApiError::forbidden("Assigned host identity required"))?;
    Ok(Authority {
        session,
        device_id,
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
    discarded_output: usize,
    terminal: Option<Terminal>,
    input_consent: Option<tasks::Consent>,
    truncated: bool,
    cancel: bool,
}

impl Run {
    fn active(&self) -> bool {
        self.status == "running"
    }

    fn page(&self, id: Uuid, cursor: usize, allowed: bool) -> Result<Value, ApiError> {
        let (end, output) = if self.output.is_empty() && cursor <= self.discarded_output {
            (cursor, &[][..])
        } else {
            let end = self.output.len().min(cursor.saturating_add(PAGE_SIZE));
            let output = self
                .output
                .get(cursor..end)
                .ok_or_else(|| ApiError::bad("Invalid output cursor"))?;
            (end, output)
        };
        Ok(json!({
            "id": id, "session": self.session, "status": self.status, "exit_code": self.exit_code,
            "access": if allowed { "ready" } else { "paused" },
            "cursor": end, "more": end < self.output.len(), "truncated": self.truncated,
            "output_base64": STANDARD.encode(output),
            "terminal": self.terminal.as_ref().map(|terminal| terminal.view(allowed && self.active() && !self.cancel)),
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

fn validate_wsl_workspace(
    workspace: &str,
    requested: Option<&str>,
    current: Option<&str>,
) -> Result<(), ApiError> {
    if requested != current {
        return Err(ApiError::forbidden("Terminal task belongs to a different WSL distribution"));
    }
    if current.is_some() && !crate::workspace_runtime::wsl_linux_path(workspace) {
        return Err(ApiError::bad("Use the Linux workspace path inside the WSL distribution"));
    }
    Ok(())
}

pub(super) async fn exchange(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Json(body): Json<RequestBody>,
) -> Result<Json<Value>, ApiError> {
    if !link.0 {
        return Err(ApiError::forbidden("Private companion connection required"));
    }
    let distro = crate::workspace_runtime::wsl_distro().map_err(ApiError::unavailable)?;
    validate_wsl_workspace(&body.workspace, body.wsl_distro.as_deref(), distro)?;
    let authority = authority(&state, &body.workspace).await?;
    if body.expected_device_id.is_some_and(|device| device != authority.device_id) {
        return Err(ApiError::forbidden("Assigned host differs from the selected enrolled device"));
    }
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
                    "interactive": d.task.interactive,
                })
            });
            let mut context = json!({
                "boot": store.boot, "workspace": body.workspace, "session": authority.session,
                "binding": authority.binding, "device_id": authority.device_id, "task": task,
                "access": if consent.is_some() { "ready" } else { "paused" },
            });
            if let Some(distro) = distro {
                context["wsl_distro"] = json!(distro);
            }
            Ok(Json(context))
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
            let consent =
                consent.ok_or_else(|| ApiError::forbidden("Local terminal access is paused"))?;
            if records.len() >= MAX_RUNS || records.values().any(Run::active) {
                return Err(ApiError::conflict("Terminal task capacity reached"));
            }
            let declared = declaration(&state.mcp_dir, &body.workspace)
                .map_err(|_| ApiError::forbidden("Terminal task declaration refused"))?
                .filter(|task| task.revision == revision)
                .ok_or_else(|| {
                    ApiError::conflict("Review the current terminal task declaration")
                })?;
            let process = {
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
                discarded_output: 0,
                terminal: process.master.clone().map(Terminal::new),
                input_consent: declared.task.interactive.then_some(consent),
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
                process,
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
        Operation::Control {
            boot,
            session,
            binding,
            id,
            terminal,
            request,
        } => {
            require_binding(store, &authority, boot, session, &binding)?;
            let revision = declaration(&state.mcp_dir, &body.workspace)
                .map_err(|_| ApiError::forbidden("Terminal task declaration refused"))?
                .ok_or_else(|| ApiError::forbidden("Terminal input declaration removed"))?
                .revision;
            let mut records = lock(&store.records);
            let run = connected_run(records.get_mut(&id), &authority)?;
            let gate = lock(&state.automation);
            if !run.active()
                || run.cancel
                || run.revision != revision
                || !state.selected_approved.load(Ordering::SeqCst)
                || !run.input_consent.is_some_and(|grant| grant.matches(&gate))
            {
                return Err(ApiError::forbidden("Terminal input access unavailable"));
            }
            let terminal = run
                .terminal
                .as_mut()
                .filter(|tty| tty.id() == terminal)
                .ok_or_else(|| ApiError::forbidden("Current interactive terminal required"))?;
            Ok(Json(terminal.control(request)?))
        }
    }
}

struct Process {
    child: tokio::process::Child,
    group: ProcessGroup,
    master: Option<Arc<Master>>,
}

fn spawn(task: &Task) -> Result<Process, ApiError> {
    let mut command = tokio::process::Command::new(&task.command);
    command
        .args(&task.args)
        .current_dir(&task.cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .kill_on_drop(true);
    let master = if task.interactive {
        process_group::validate_interactive()
            .map_err(|_| ApiError::unavailable("Terminal session cleanup is unavailable"))?;
        Some(Master::attach(&mut command).map_err(|_| {
            ApiError::unavailable("Could not open a private terminal for this task")
        })?)
    } else {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        None
    };
    let child = command
        .spawn()
        .map_err(|_| ApiError::unavailable("Could not start the declared terminal task"))?;
    let group = ProcessGroup::new(
        child.id().expect("spawned child has an ID") as i32,
        task.interactive,
    );
    Ok(Process {
        child,
        group,
        master,
    })
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
    process: Process,
) {
    let Process {
        mut child,
        master,
        group,
    } = process;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out = [0; 4096];
    let mut err = [0; 4096];
    let (mut out_open, mut err_open) = (true, stderr.is_some());
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
            read = async {
                if let Some(pty) = &master {
                    pty.read(&mut out).await
                } else {
                    stdout.as_mut().expect("configured stdout").read(&mut out).await
                }
            }, if out_open => match read {
                Ok(0) => out_open = false,
                Ok(count) => if let Some(run) = lock(&state.terminal_tasks.records).get_mut(&id) {
                    append_output(run, &out[..count]);
                },
                Err(_) => break "failed",
            },
            read = async { stderr.as_mut().expect("configured stderr").read(&mut err).await }, if err_open => match read {
                Ok(0) => err_open = false,
                Ok(count) => if let Some(run) = lock(&state.terminal_tasks.records).get_mut(&id) {
                    append_output(run, &err[..count]);
                },
                Err(_) => break "failed",
            },
        }
    };
    if let Some(run) = lock(&state.terminal_tasks.records).get_mut(&id) {
        run.cancel = true;
        if let Some(terminal) = run.terminal.as_mut() {
            terminal.close();
        }
        if status == "revoked" {
            run.discarded_output = run.output.len();
            run.output.clear();
        }
    }
    let stopped = matches!(
        tokio::task::spawn_blocking(move || group.stop()).await,
        Ok(Ok(()))
    );
    let exit = child.wait().await.ok();
    let mut code = None;
    if !stopped {
        status = "failed";
    } else if status == "exited" {
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
        call_device(app, workspace, operation, None).await
    }

    async fn call_device(
        app: &Router,
        workspace: &std::path::Path,
        operation: Value,
        expected: Option<Uuid>,
    ) -> (StatusCode, Value) {
        let distro = crate::workspace_runtime::wsl_distro().unwrap();
        trust_test_request(
            app.clone(),
            "POST",
            "/terminal-task",
            json!({"workspace":workspace,"wsl_distro":distro,"operation":operation,"expected_device_id":expected}),
        )
        .await
    }

    #[test]
    fn terminal_scope_rejects_wrong_wsl_distribution_and_windows_paths() {
        let workspace = "/home/dev/project";
        assert!(validate_wsl_workspace(workspace, None, None).is_ok());
        assert!(validate_wsl_workspace(workspace, Some("Ubuntu"), Some("Ubuntu")).is_ok());
        assert!(validate_wsl_workspace(workspace, Some("Debian"), Some("Ubuntu")).is_err());
        assert!(validate_wsl_workspace(workspace, None, Some("Ubuntu")).is_err());
        assert!(validate_wsl_workspace(workspace, Some("Ubuntu"), None).is_err());
        assert!(validate_wsl_workspace("C:/project", Some("Ubuntu"), Some("Ubuntu")).is_err());
    }

    #[test]
    fn revoked_output_keeps_issued_cursors_without_revealing_bytes() {
        let run = Run {
            session: Uuid::new_v4(),
            binding: String::new(),
            revision: String::new(),
            status: "revoked",
            exit_code: None,
            output: Vec::new(),
            discarded_output: 32,
            terminal: None,
            input_consent: None,
            truncated: false,
            cancel: false,
        };
        let id = Uuid::new_v4();
        for cursor in [0, 17, 32] {
            let page = run.page(id, cursor, true).ok().expect("issued cursor");
            assert_eq!(page["status"], "revoked");
            assert_eq!(page["cursor"], cursor);
            assert_eq!(page["output_base64"], "");
            assert_eq!(page["more"], false);
        }
        assert!(run.page(id, 33, true).is_err());
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
        let device = Uuid::new_v4();
        let identity = json!({
            "workspace":workspace, "identity":{
                "session_id":session,"tenant_id":Uuid::new_v4(),"device_id":device,
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
        let write_task = |command: &str, args: &[&str], interactive: bool| {
            if config.exists() {
                fs::remove_file(&config).unwrap();
            }
            let mut output =
                hypermesh_local_storage::open(&config, hypermesh_local_storage::Open::New).unwrap();
            writeln!(
                output,
                "{}",
                json!({"workspace":workspace,"cwd":cwd,
                "command":command,"args":args,"timeout_seconds":10,"interactive":interactive})
            )
            .unwrap();
        };
        write_task("/bin/sh", &["-c", "pwd; printf x >> launches"], false);
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
        assert_eq!(scope["device_id"], json!(device));
        let context = json!({"type":"context"});
        assert_eq!(
            call_device(&app, &workspace, context.clone(), Some(device)).await,
            (StatusCode::OK, scope.clone())
        );
        assert_eq!(
            call_device(&app, &workspace, context.clone(), Some(Uuid::new_v4())).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call_device(&app, &directory, context, Some(device)).await.0,
            StatusCode::FORBIDDEN
        );

        let id = Uuid::new_v4();
        let base = |kind: &str, id: Uuid| json!({"type":kind,"boot":scope["boot"],"session":session,"binding":scope["binding"],"id":id});
        let mut start = base("start", id);
        start["revision"] = scope["task"]["revision"].clone();
        let (status, cli) = call(&app, &workspace, start.clone()).await;
        assert_eq!(status, StatusCode::OK, "{cli}");
        assert_eq!(
            call_device(&app, &workspace, start.clone(), Some(Uuid::new_v4())).await.0,
            StatusCode::FORBIDDEN
        );
        let (status, editor) = call_device(&app, &workspace, start, Some(device)).await;
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
        assert_eq!(
            call_device(&app, &workspace, read.clone(), Some(Uuid::new_v4())).await.0,
            StatusCode::FORBIDDEN
        );
        let (_, attached) = call_device(&app, &workspace, read.clone(), Some(device)).await;
        assert_eq!(attached["id"], finished["id"]);
        assert_eq!(attached["exit_code"], finished["exit_code"]);
        assert_eq!(attached["output_base64"], "");

        write_task("/bin/sleep", &["10"], false);
        let (_, updated) = call(&app, &workspace, json!({"type":"context"})).await;
        let active = Uuid::new_v4();
        let mut start = base("start", active);
        start["revision"] = updated["task"]["revision"].clone();
        assert_eq!(call(&app, &workspace, start).await.0, StatusCode::OK);
        let control = |id: Uuid, terminal: Value, request: Value| {
            let mut body = base("control", id);
            body["terminal"] = terminal;
            body["request"] = request;
            body
        };
        let editor = Uuid::new_v4();
        let claim = |owner: Uuid, client, previous: Value| json!({"type":"claim","owner":owner,"client":client,"expected_owner":previous});
        assert_eq!(
            call(
                &app,
                &workspace,
                control(
                    active,
                    json!(Uuid::new_v4()),
                    claim(editor, "editor", Value::Null)
                )
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
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
        write_task(
            "/bin/sh",
            &[
                "-c",
                r#"printf 'ready\n'; IFS= read -r line; printf 'first=%s\n' "$line"; stty size; IFS= read -r again; printf 'second=%s\n' "$again"; sleep 10"#,
            ],
            true,
        );
        let (_, interactive) = call(&app, &workspace, json!({"type":"context"})).await;
        let active = Uuid::new_v4();
        let mut start = base("start", active);
        start["revision"] = interactive["task"]["revision"].clone();
        let (status, page) = call(&app, &workspace, start).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let terminal = page["terminal"]["id"].clone();
        assert_eq!(page["terminal"]["input_available"], true);
        let mut watch = base("read", active);
        watch["cursor"] = json!(0);
        let tty = |request| control(active, terminal.clone(), request);
        let cli = Uuid::new_v4();
        let (status, accepted) =
            call(&app, &workspace, tty(claim(editor, "editor", Value::Null))).await;
        assert_eq!(status, StatusCode::OK, "{accepted}");
        let observed = call(&app, &workspace, watch.clone()).await.1;
        assert_eq!(observed["terminal"]["owner"]["id"], json!(editor));
        assert!(observed["terminal"]["owner"].get("lease").is_none());
        let lease = accepted["lease"].clone();
        let resize = json!({"type":"resize","lease":lease,"sequence":1,"rows":37,"cols":101});
        let (status, ack) = call(&app, &workspace, tty(resize)).await;
        assert_eq!(status, StatusCode::OK, "{ack}");
        assert_eq!(ack["ack"]["resized"], true);
        let input = json!({"type":"input","lease":lease,"sequence":2,"data_base64":STANDARD.encode(b"sample\n")});
        let (status, ack) = call(&app, &workspace, tty(input.clone())).await;
        assert_eq!(status, StatusCode::OK, "{ack}");
        assert_eq!(ack["ack"]["accepted_bytes"], 7);
        assert_eq!(
            call(&app, &workspace, tty(input)).await,
            (StatusCode::OK, ack)
        );
        assert_eq!(
            call(&app, &workspace, tty(claim(cli, "cli", Value::Null)))
                .await
                .0,
            StatusCode::CONFLICT
        );
        let (status, takeover) =
            call(&app, &workspace, tty(claim(cli, "cli", json!(editor)))).await;
        assert_eq!(status, StatusCode::OK, "{takeover}");
        assert_eq!(takeover["terminal"]["owner"]["client"], "cli");
        assert_eq!(takeover["terminal"]["owner"]["ack"], Value::Null);
        let stale = json!({"type":"input","lease":lease,"sequence":3,"data_base64":STANDARD.encode(b"stale\n")});
        assert_eq!(
            call(&app, &workspace, tty(stale)).await.0,
            StatusCode::FORBIDDEN
        );
        let cli_lease = takeover["lease"].clone();
        let send = |sequence, text: &str| json!({"type":"input","lease":cli_lease,"sequence":sequence,"data_base64":STANDARD.encode(text)});
        assert_eq!(
            call(&app, &workspace, tty(send(1, "finish\n"))).await.0,
            StatusCode::OK
        );
        let text = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (status, result) = call(&app, &workspace, watch.clone()).await;
                assert_eq!(status, StatusCode::OK, "{result}");
                let bytes = STANDARD
                    .decode(result["output_base64"].as_str().unwrap())
                    .unwrap();
                let text = String::from_utf8(bytes).unwrap();
                if text.contains("second=") {
                    break text;
                }
                tokio::time::sleep(TICK).await;
            }
        })
        .await
        .unwrap();
        assert!(
            text.contains("first=sample")
                && text.contains("37 101")
                && text.contains("second=finish"),
            "{text}"
        );
        assert!(!text.contains("stale"));
        assert!(state.control_automation(Control::Pause, true, None).is_ok());
        assert_eq!(
            call(&app, &workspace, tty(send(2, "paused\n"))).await.0,
            StatusCode::FORBIDDEN
        );
        let (_, paused) = call(&app, &workspace, read.clone()).await;
        assert_eq!(paused["access"], "paused");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (_, page) = call(&app, &workspace, watch.clone()).await;
                if page["status"] == "paused" {
                    assert_eq!(page["terminal"]["input_available"], false);
                    assert_eq!(page["terminal"]["owner"], Value::Null);
                    break;
                }
                tokio::time::sleep(TICK).await;
            }
        })
        .await
        .unwrap();
        let mut refused = base("start", Uuid::new_v4());
        refused["revision"] = updated["task"]["revision"].clone();
        assert_eq!(
            call(&app, &workspace, refused).await.0,
            StatusCode::FORBIDDEN
        );
        let epoch = lock(&state.automation).epoch();
        assert!(state
            .control_automation_at(Control::Resume, true, Some(session), Some(epoch))
            .is_ok());
        let second = Uuid::new_v4();
        let mut start = base("start", second);
        start["revision"] = interactive["task"]["revision"].clone();
        let (status, page) = call(&app, &workspace, start).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let tty = |request| control(second, page["terminal"]["id"].clone(), request);
        let (_, granted) = call(&app, &workspace, tty(claim(editor, "editor", Value::Null))).await;
        let first_lease = granted["lease"].clone();
        let (status, detached) = call(
            &app,
            &workspace,
            tty(json!({"type":"release","lease":first_lease})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{detached}");
        assert_eq!(detached["terminal"]["owner"], Value::Null);
        let mut reread = base("read", second);
        reread["cursor"] = json!(0);
        assert_eq!(
            call(&app, &workspace, reread.clone()).await.1["status"],
            "running"
        );
        let (status, reattached) =
            call(&app, &workspace, tty(claim(cli, "cli", Value::Null))).await;
        assert_eq!(status, StatusCode::OK, "{reattached}");
        assert_eq!(reattached["terminal"]["owner"]["ack"], Value::Null);
        let (status, requested) = call(&app, &workspace, base("cancel", second)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(requested["terminal"]["input_available"], false);
        let queued =
            json!({"type":"input","lease":reattached["lease"],"sequence":1,"data_base64":"YQ=="});
        assert_eq!(
            call(&app, &workspace, tty(queued)).await.0,
            StatusCode::FORBIDDEN
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (_, page) = call(&app, &workspace, reread.clone()).await;
                if page["status"] == "cancelled" {
                    break;
                }
                tokio::time::sleep(TICK).await;
            }
        })
        .await
        .unwrap();
        lock(&state.sessions).remove(&session);
        assert_eq!(call(&app, &workspace, read).await.0, StatusCode::FORBIDDEN);
        fixture.abort();
    }
}
