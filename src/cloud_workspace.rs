//! Cloud workspace clients use the same private worker connection.
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::Path,
};
use uuid::Uuid;

pub(crate) const ROUTE: &str = "/cloud-workspace-task";
pub(crate) const CONDUCTOR_ROUTE: &str = "/v1/workspace-task";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    device_id: Uuid,
    worker_generation: u64,
    task_id: Uuid,
    lease_id: Uuid,
    workspace: String,
    operation: Operation,
    #[serde(default)]
    cursor: usize,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Inspect,
    Read,
    Invoke,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct View {
    version: u8,
    device_id: Uuid,
    worker_generation: u64,
    task_id: Uuid,
    lease_id: Uuid,
    workspace: String,
    expires_at_ms: u64,
    tool: String,
    target: String,
    state: State,
    output: String,
    cursor: usize,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Ready,
    Running,
    Completed,
    Failed,
    Revoked,
}

impl Request {
    pub(crate) fn valid(&self) -> bool {
        !self.device_id.is_nil()
            && self.worker_generation > 0
            && !self.task_id.is_nil()
            && !self.lease_id.is_nil()
            && self.workspace.len() <= 4096
            && !self.workspace.chars().any(char::is_control)
            && Path::new(&self.workspace).canonicalize().ok().as_deref()
                == Some(Path::new(&self.workspace))
            && Path::new(&self.workspace).is_dir()
            && self.cursor <= 4096
            && (self.operation == Operation::Read || self.cursor == 0)
    }
}

impl View {
    pub(crate) fn matches(&self, request: &Request) -> bool {
        self.version == 1
            && self.device_id == request.device_id
            && self.worker_generation == request.worker_generation
            && self.task_id == request.task_id
            && self.lease_id == request.lease_id
            && self.workspace == request.workspace
            && crate::companion_lease::now_ms().is_some_and(|now| self.expires_at_ms > now)
            && self.tool == "check-json"
            && !self.target.is_empty()
            && self.target.len() <= 128
            && self
                .target
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            && self.output.len() <= 4096
            && self.output.chars().all(|c| c == '\n' || !c.is_control())
            && self.cursor == request.cursor + self.output.len()
            && (request.operation == Operation::Read || self.cursor == 0)
    }
}

const USAGE: &str = "workspace-task inspect|invoke|watch WORKSPACE DEVICE GENERATION TASK LEASE [CURSOR]\nworkspace-task request WORKSPACE";

pub fn cli(args: &[String], input: impl Read, mut output: impl Write) -> Result<bool, String> {
    let (mut request, watch) = match args {
        [command, workspace] if command == "request" => {
            let mut bytes = Vec::new();
            input
                .take(8193)
                .read_to_end(&mut bytes)
                .map_err(|_| "Cannot read workspace request")?;
            if bytes.len() > 8192 {
                return Err("Workspace request too large".into());
            }
            let value: Request =
                serde_json::from_slice(&bytes).map_err(|_| "Invalid workspace request")?;
            if value.workspace != *workspace {
                return Err("Workspace request differs from the selected folder".into());
            }
            (value, false)
        }
        [command, workspace, device, generation, task, lease, tail @ ..] => {
            let operation = match (command.as_str(), tail) {
                ("inspect", []) => Operation::Inspect,
                ("invoke", []) => Operation::Invoke,
                ("watch", [] | [_]) => Operation::Read,
                _ => return Err(USAGE.into()),
            };
            let uuid = |id: &str| Uuid::parse_str(id).map_err(|_| "Invalid workspace identifier");
            let cursor = tail
                .first()
                .map(|text| text.parse())
                .transpose()
                .map_err(|_| "Invalid workspace cursor")?
                .unwrap_or(0);
            (
                Request {
                    device_id: uuid(device)?,
                    worker_generation: generation
                        .parse()
                        .map_err(|_| "Invalid worker generation")?,
                    task_id: uuid(task)?,
                    lease_id: uuid(lease)?,
                    workspace: workspace.clone(),
                    operation,
                    cursor,
                },
                operation == Operation::Read,
            )
        }
        _ => return Err(USAGE.into()),
    };
    if !request.valid() {
        return Err("Invalid cloud workspace scope".into());
    }
    let runtime_dir =
        crate::local_runtime::runtime_dir().map_err(|_| "Private worker socket unavailable")?;
    let socket = crate::local_runtime::socket_file(&runtime_dir);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "Cannot start workspace client")?;
    loop {
        let reply: View = runtime.block_on(crate::conductor_socket::private_json(
            &socket, ROUTE, &request,
        ))?;
        if !reply.matches(&request) {
            return Err("Cloud workspace scope changed".into());
        }
        if !watch {
            serde_json::to_writer(&mut output, &reply)
                .map_err(|_| "Cannot write workspace state")?;
            writeln!(output).map_err(|_| "Cannot write workspace state")?;
            return Ok(reply.state != State::Revoked);
        }
        output
            .write_all(reply.output.as_bytes())
            .map_err(|_| "Cannot write workspace output")?;
        output
            .flush()
            .map_err(|_| "Cannot write workspace output")?;
        match reply.state {
            State::Running => {
                request.cursor = reply.cursor;
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            State::Completed => return Ok(true),
            State::Failed | State::Revoked => return Ok(false),
            State::Ready => return Err("Invoke the declared workspace tool before watching".into()),
        }
    }
}
