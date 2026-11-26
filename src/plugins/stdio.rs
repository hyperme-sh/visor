use super::{stage_file, Error, Package, Profile, HOST_VERSIONS};
use crate::mcp::protocol;
use serde_json::{json, Value};
use std::{
    fs::File,
    io::Write,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::fs::{FileExt, MetadataExt},
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

const MAX_FRAME: usize = 96 * 1024;
const BWRAP: &str = "/usr/bin/bwrap";

pub(crate) struct Channel {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Channel {
    pub(crate) async fn send(&mut self, frame: &Value) -> Result<(), Error> {
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
        let value = self.receive().await?;
        protocol::validate_response(&value, &json!(id)).map_err(|_| Error::InvalidOutput)?;
        Ok(value)
    }

    pub(crate) async fn receive(&mut self) -> Result<Value, Error> {
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
        serde_json::from_slice(&raw).map_err(|_| Error::InvalidOutput)
    }
}

pub(crate) fn check_sandbox() -> Result<(), Error> {
    let bwrap = std::fs::symlink_metadata(BWRAP).map_err(|_| Error::Unavailable)?;
    if !bwrap.is_file()
        || bwrap.uid() != 0
        || bwrap.mode() & 0o6022 != 0
        || unsafe { libc::geteuid() } == 0
    {
        return Err(Error::Unavailable);
    }
    Ok(())
}

pub(crate) enum WorkerImage {
    Staged(super::Staged),
    Sealed(File),
}

fn prepare(staging: &Path, bytes: &[u8], bundled: bool) -> Result<WorkerImage, Error> {
    if bundled {
        if unsafe { libc::geteuid() } == 0 {
            return Err(Error::Unavailable);
        }
        return sealed_worker(bytes).map(WorkerImage::Sealed);
    }
    check_sandbox()?;
    stage_file(staging, bytes).map(WorkerImage::Staged)
}

fn sealed_worker(bytes: &[u8]) -> Result<File, Error> {
    // The session owner can mutate scratch paths. Execute only the verified
    // bytes from an anonymous file that cannot be rewritten or re-sealed.
    let fd = unsafe {
        libc::memfd_create(
            c"visor-bundled-plugin".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(Error::Unavailable);
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes).map_err(|_| Error::Unavailable)?;
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    if unsafe { libc::fchmod(fd, 0o500) } != 0
        || unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) } != 0
    {
        return Err(Error::Unavailable);
    }
    // Authentication must cover the frozen bytes, including during staging.
    if file.metadata().map_err(|_| Error::Unavailable)?.len() != bytes.len() as u64 {
        return Err(Error::Unavailable);
    }
    const BLOCK: usize = 8192;
    let mut buffer = [0; BLOCK];
    for (index, chunk) in bytes.chunks(BLOCK).enumerate() {
        let read = &mut buffer[..chunk.len()];
        file.read_exact_at(read, (index * BLOCK) as u64)
            .map_err(|_| Error::Unavailable)?;
        if read != chunk {
            return Err(Error::Unavailable);
        }
    }
    Ok(file)
}

pub(crate) fn sandbox_command(
    executable: &Path,
    files: &[(&Path, &str)],
    arguments: &[&str],
) -> Command {
    let mut command = Command::new(BWRAP);
    command
        .args([
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
            "--cap-drop",
            "ALL",
        ])
        .arg("--ro-bind")
        .arg(executable)
        .arg("/plugin");
    for (source, target) in files {
        command.arg("--ro-bind").arg(source).arg(target);
    }
    command
        .args(["--remount-ro", "/", "--chdir", "/", "--", "/plugin"])
        .args(arguments);
    configure_worker(&mut command);
    command
}

fn configure_worker(command: &mut Command) {
    command
        .env_clear()
        .current_dir("/")
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
}

async fn launch(executable: &WorkerImage, arguments: &[&str]) -> Result<(Child, Channel), Error> {
    let mut command = match executable {
        WorkerImage::Sealed(file) => {
            // CLOEXEC keeps the descriptor usable to resolve the ELF at exec
            // time and removes it before the worker starts accepting input.
            let mut command = Command::new(format!("/proc/self/fd/{}", file.as_raw_fd()));
            configure_worker(&mut command);
            command.arg("--sandbox-v1").args(arguments);
            command
        }
        WorkerImage::Staged(file) => sandbox_command(&file.0, &[], arguments),
    };
    let mut child = command.spawn().map_err(|_| Error::Unavailable)?;
    let mut channel = Channel {
        stdin: child.stdin.take().ok_or(Error::Unavailable)?,
        stdout: BufReader::new(child.stdout.take().ok_or(Error::Unavailable)?),
    };
    // The image-owned worker installs its filter before accepting a frame.
    if matches!(executable, WorkerImage::Sealed(_))
        && channel.receive().await? != json!({"sandbox": 1})
    {
        return Err(Error::Incompatible);
    }
    Ok((child, channel))
}

pub(crate) async fn start_code(
    package: &Package,
    staging: &Path,
) -> Result<(Child, Channel, WorkerImage), Error> {
    let staging = staging.to_owned();
    let bytes = package.executable.clone();
    let bundled = package.bundled;
    let executable = tokio::task::spawn_blocking(move || {
        hypermesh_local_storage::create_dir(&staging).map_err(|_| Error::Unavailable)?;
        prepare(&staging, &bytes, bundled)
    })
    .await
    .map_err(|_| Error::Unavailable)??;
    let (child, channel) = launch(&executable, &["--code-mode-v1"]).await?;
    Ok((child, channel, executable))
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
    let bundled = package.bundled;
    let executable = tokio::task::spawn_blocking(move || prepare(&staging, &bytes, bundled))
        .await
        .map_err(|_| Error::Unavailable)??;
    if !live() {
        return Err(Error::Cancelled);
    }
    let (mut child, mut channel) = launch(&executable, &[]).await?;
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
    // Finish the worker even when the caller is no longer reading.
    let _ = child.start_kill();
    response
}
