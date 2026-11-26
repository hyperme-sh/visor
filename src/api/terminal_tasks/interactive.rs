use super::*;
use sha2::Sha256;
use std::{
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd},
};
use tokio::{io::unix::AsyncFd, process::Command};

const MAX_INPUT: usize = 1024;
const MAX_DIMENSION: u16 = 1000;

pub(super) struct Master(AsyncFd<File>);

impl Master {
    pub(super) fn attach(command: &mut Command) -> io::Result<Arc<Self>> {
        let (master, slave) = open_terminal()?;
        let master = Arc::new(Self(AsyncFd::new(master)?));
        master.resize(24, 80)?;
        command
            .env("TERM", "dumb")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // After stdio is installed, make the slave the controlling terminal of
        // this child session. Only async-signal-safe syscalls run before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(master)
    }

    pub(super) async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut ready = self.0.readable().await?;
            match ready.try_io(|fd| {
                let mut file = fd.get_ref();
                file.read(buffer)
            }) {
                // Linux signals that the last slave closed by returning EIO.
                Ok(Err(error)) if error.raw_os_error() == Some(libc::EIO) => return Ok(0),
                Ok(result) => return result,
                Err(_) => continue,
            }
        }
    }

    fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        let mut file = self.0.get_ref();
        match file.write(bytes) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(0),
            result => result,
        }
    }

    fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(self.0.as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn open_file(fd: libc::c_int) -> io::Result<File> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Each syscall returns a newly owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_terminal() -> io::Result<(File, File)> {
    let master = open_file(unsafe {
        libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NONBLOCK)
    })?;
    let fd = master.as_raw_fd();
    if unsafe { libc::grantpt(fd) } < 0 || unsafe { libc::unlockpt(fd) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = open_slave(fd)?;
    Ok((master, slave))
}

#[cfg(target_os = "linux")]
fn open_slave(fd: libc::c_int) -> io::Result<File> {
    open_file(unsafe {
        libc::ioctl(
            fd,
            libc::TIOCGPTPEER,
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    })
}

#[cfg(not(target_os = "linux"))]
fn open_slave(fd: libc::c_int) -> io::Result<File> {
    // Darwin supplies the slave path in memory owned by libc.
    static NAME: Mutex<()> = Mutex::new(());
    let _guard = lock(&NAME);
    let name = unsafe { libc::ptsname(fd) };
    if name.is_null() {
        return Err(io::Error::last_os_error());
    }
    open_file(unsafe { libc::open(name, libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) })
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Client {
    Editor,
    Cli,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ControlRequest {
    Claim {
        owner: Uuid,
        client: Client,
        expected_owner: Option<Uuid>,
    },
    Release {
        lease: Uuid,
    },
    Input {
        lease: Uuid,
        sequence: u64,
        data_base64: String,
    },
    Resize {
        lease: Uuid,
        sequence: u64,
        rows: u16,
        cols: u16,
    },
}

#[derive(Serialize)]
struct Ack {
    sequence: u64,
    accepted_bytes: Option<usize>,
    resized: bool,
    #[serde(skip)]
    fingerprint: [u8; 32],
}

struct Owner {
    id: Uuid,
    client: Client,
    lease: Uuid,
    ack: Option<Ack>,
}

pub(super) struct Terminal {
    id: Uuid,
    master: Option<Arc<Master>>,
    owner: Option<Owner>,
}

impl Terminal {
    pub(super) fn new(master: Arc<Master>) -> Self {
        Self {
            id: Uuid::new_v4(),
            master: Some(master),
            owner: None,
        }
    }

    pub(super) fn id(&self) -> Uuid {
        self.id
    }

    pub(super) fn close(&mut self) {
        self.owner = None;
        self.master = None;
    }

    pub(super) fn view(&self, allowed: bool) -> Value {
        json!({
            "id":self.id,
            "input_available":allowed && self.master.is_some(),
            "owner":self.owner.as_ref().map(|o| json!({"id":o.id,"client":o.client,"ack":o.ack})),
        })
    }

    pub(super) fn control(&mut self, request: ControlRequest) -> Result<Value, ApiError> {
        let master = self
            .master
            .as_ref()
            .ok_or_else(|| ApiError::conflict("Terminal task has stopped"))?;
        match request {
            ControlRequest::Claim {
                owner,
                client,
                expected_owner,
            } => {
                if owner.is_nil() || self.owner.as_ref().map(|o| o.id) != expected_owner {
                    return Err(ApiError::conflict("Terminal input ownership changed"));
                }
                let lease = Uuid::new_v4();
                self.owner = Some(Owner {
                    id: owner,
                    client,
                    lease,
                    ack: None,
                });
                Ok(json!({"lease":lease,"terminal":self.view(true)}))
            }
            ControlRequest::Release { lease } => {
                require_owner(&mut self.owner, lease)?;
                self.owner = None;
                Ok(json!({"terminal":self.view(true)}))
            }
            ControlRequest::Input {
                lease,
                sequence,
                data_base64,
            } => {
                let owner = require_owner(&mut self.owner, lease)?;
                if data_base64.len() > MAX_INPUT.div_ceil(3) * 4 {
                    return Err(ApiError::bad("Terminal input exceeds its limit"));
                }
                let bytes = STANDARD
                    .decode(data_base64)
                    .map_err(|_| ApiError::bad("Invalid terminal input"))?;
                if bytes.is_empty() || bytes.len() > MAX_INPUT {
                    return Err(ApiError::bad("Invalid terminal input length"));
                }
                let fingerprint = hash_action(1, &bytes);
                acknowledge(owner, sequence, fingerprint, || {
                    master.write(&bytes).map(Some)
                })
            }
            ControlRequest::Resize {
                lease,
                sequence,
                rows,
                cols,
            } => {
                let owner = require_owner(&mut self.owner, lease)?;
                if !(1..=MAX_DIMENSION).contains(&rows) || !(1..=MAX_DIMENSION).contains(&cols) {
                    return Err(ApiError::bad("Invalid terminal dimensions"));
                }
                let [row_hi, row_lo] = rows.to_be_bytes();
                let [col_hi, col_lo] = cols.to_be_bytes();
                let fingerprint = hash_action(2, &[row_hi, row_lo, col_hi, col_lo]);
                acknowledge(owner, sequence, fingerprint, || {
                    master.resize(rows, cols).map(|()| None)
                })
            }
        }
    }
}

fn require_owner(owner: &mut Option<Owner>, lease: Uuid) -> Result<&mut Owner, ApiError> {
    owner
        .as_mut()
        .filter(|o| o.lease == lease)
        .ok_or_else(|| ApiError::forbidden("Terminal input belongs to a different attachment"))
}

fn hash_action(kind: u8, args: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update([kind]);
    digest.update(args);
    digest.finalize().into()
}

fn acknowledge(
    owner: &mut Owner,
    sequence: u64,
    fingerprint: [u8; 32],
    apply: impl FnOnce() -> io::Result<Option<usize>>,
) -> Result<Value, ApiError> {
    if let Some(last) = &owner.ack {
        if last.sequence == sequence && last.fingerprint == fingerprint {
            return Ok(json!({"ack":last}));
        }
    }
    let last = owner.ack.as_ref().map_or(0, |ack| ack.sequence);
    if last.checked_add(1) != Some(sequence) || sequence > 9_007_199_254_740_991 {
        return Err(ApiError::conflict("Terminal input sequence changed"));
    }
    let accepted_bytes =
        apply().map_err(|_| ApiError::unavailable("Terminal operation could not be applied"))?;
    let ack = Ack {
        sequence,
        accepted_bytes,
        resized: accepted_bytes.is_none(),
        fingerprint,
    };
    let response = json!({"ack":ack});
    owner.ack = Some(ack);
    Ok(response)
}
