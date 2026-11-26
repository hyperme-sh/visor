use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const DIRECTORY: libc::c_int = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

pub(super) struct Directory(File);

pub(crate) fn open_root(config: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(DIRECTORY)
        .open(config.join("companion-packs"))
}

impl Directory {
    pub(super) fn open(config: &Path, id: &str) -> io::Result<Self> {
        let root = open_root(config)?;
        open_child(&root, id, DIRECTORY).map(Self)
    }

    pub(super) fn asset(&self, name: &str) -> io::Result<File> {
        open_child(
            &self.0,
            name,
            libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    }
}

fn open_child(parent: &File, name: &str, flags: libc::c_int) -> io::Result<File> {
    let name = CString::new(name).map_err(io::Error::other)?;
    // The parent descriptor and NUL-terminated name stay live across openat.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags | libc::O_RDONLY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
