use super::{stage_file, Error, Package, Profile, HOST_VERSIONS};
use crate::mcp::protocol;
use serde_json::{json, Value};
use std::{
    os::unix::fs::MetadataExt,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
};

const MAX_FRAME: usize = 96 * 1024;
const BWRAP: &str = "/usr/bin/bwrap";

struct Channel {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Channel {
    async fn send(&mut self, frame: &Value) -> Result<(), Error> {
        let mut raw = serde_json::to_vec(frame).map_err(|_| Error::InvalidInput)?;
        raw.push(b'\n');
        if raw.len() > MAX_FRAME {
            return Err(Error::InvalidInput);
        }
        self.stdin
            .write_all(&raw)
            .await
            .map_err(|_| Error::Unavailable)
    }

    async fn request(&mut self, id: u32, method: &str, params: Value) -> Result<Value, Error> {
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        let mut raw = Vec::new();
        (&mut self.stdout)
            .take(MAX_FRAME as u64 + 1)
            .read_until(b'\n', &mut raw)
            .await
            .map_err(|_| Error::Unavailable)?;
        if raw.is_empty() {
            return Err(Error::Unavailable);
        }
        if raw.len() > MAX_FRAME || raw.last() != Some(&b'\n') {
            return Err(Error::InvalidOutput);
        }
        let value: Value = serde_json::from_slice(&raw).map_err(|_| Error::InvalidOutput)?;
        protocol::validate_response(&value, &json!(id)).map_err(|_| Error::InvalidOutput)?;
        Ok(value)
    }
}

fn prepare(staging: &Path, bytes: &[u8]) -> Result<super::Staged, Error> {
    let bwrap = std::fs::symlink_metadata(BWRAP).map_err(|_| Error::Unavailable)?;
    if !bwrap.is_file()
        || bwrap.uid() != 0
        || bwrap.mode() & 0o6022 != 0
        || unsafe { libc::geteuid() } == 0
    {
        return Err(Error::Unavailable);
    }
    stage_file(staging, bytes)
}

pub(super) async fn invoke(
    package: &Package,
    staging: &Path,
    input: Value,
    duration: Duration,
    live: &impl Fn() -> bool,
) -> Result<Value, Error> {
    let started = Instant::now();
    let staging = staging.to_owned();
    let bytes = package.executable.clone();
    let executable = tokio::task::spawn_blocking(move || prepare(&staging, &bytes))
        .await
        .map_err(|_| Error::Unavailable)??;
    if !live() {
        return Err(Error::Cancelled);
    }
    let mut command = Command::new(BWRAP);
    command
        .env_clear()
        .args([
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
            "--cap-drop",
            "ALL",
            "--ro-bind",
        ])
        .arg(&executable.0)
        .args([
            "/plugin",
            "--remount-ro",
            "/",
            "--chdir",
            "/",
            "--",
            "/plugin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Only pipes 0–2 reach the sandbox. Run async-signal-safe syscalls here.
    unsafe {
        command.pre_exec(|| {
            if libc::syscall(
                libc::SYS_close_range,
                3u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) != 0
                || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            for (resource, bound) in [
                (libc::RLIMIT_AS, 256 * 1024 * 1024),
                (libc::RLIMIT_CPU, 10),
                (libc::RLIMIT_CORE, 0),
            ] {
                let mut limits = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(resource, &mut limits) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                limits.rlim_max = limits.rlim_max.min(bound);
                limits.rlim_cur = limits.rlim_cur.min(limits.rlim_max);
                if libc::setrlimit(resource, &limits) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|_| Error::Unavailable)?;
    let mut channel = Channel {
        stdin: child.stdin.take().ok_or(Error::Unavailable)?,
        stdout: BufReader::new(child.stdout.take().ok_or(Error::Unavailable)?),
    };
    let response = match package.manifest.profile {
        Profile::LocalStdio => {
            // Offer the highest common version first. No silent downgrade on handshake failure.
            let versions: Vec<_> = HOST_VERSIONS
                .into_iter()
                .filter(|v| package.manifest.api_versions.contains(v))
                .collect();
            let init = channel
                .request(
                    1,
                    "initialize",
                    json!({
                        "plugin_id":package.manifest.id,"api_versions":versions
                    }),
                )
                .await?;
            if init["result"]["plugin_id"] != package.manifest.id
                || init["result"]["api_version"].as_u64() != Some(u64::from(package.version))
            {
                return Err(Error::Incompatible);
            }
            if !live() {
                return Err(Error::Cancelled);
            }
            let mut params = json!({"name":package.tool.name,"input":input});
            if package.version == 1 {
                let remaining = duration.saturating_sub(started.elapsed()).as_millis();
                if remaining == 0 {
                    return Err(Error::Timeout);
                }
                params["timeout_ms"] = json!(remaining as u64);
            }
            let result = channel.request(2, "tools/call", params).await?;
            result.get("result").cloned().ok_or(Error::ToolFailed)
        }
        Profile::LocalMcpStdio => {
            let init = channel
                .request(
                    1,
                    "initialize",
                    json!({
                        "protocolVersion":protocol::VERSION,
                        "capabilities":{},
                        "clientInfo":{"name":"visor-plugin","version":"1"}
                    }),
                )
                .await?;
            let negotiated = protocol::negotiate(&init).map_err(|_| Error::Incompatible)?;
            if !negotiated.tools || negotiated.version != protocol::VERSION {
                return Err(Error::Incompatible);
            }
            channel
                .send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .await?;
            if !live() {
                return Err(Error::Cancelled);
            }
            let result = channel
                .request(
                    2,
                    "tools/call",
                    json!({
                        "name":package.tool.name,"arguments":input
                    }),
                )
                .await?;
            if result.get("error").is_some() || result["result"]["isError"] == true {
                return Err(Error::ToolFailed);
            }
            result["result"]
                .get("structuredContent")
                .cloned()
                .ok_or(Error::InvalidOutput)
        }
    };
    // Closing the sandbox leader kills the PID namespace and its descendants.
    let _ = child.start_kill();
    response
}
