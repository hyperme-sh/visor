//! Restrict the image's static worker before it reads any input.
use std::io;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) fn install() -> io::Result<()> {
    use libc::sock_filter;

    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc000_00b7;
    const LOAD_WORD: u16 = 0x20;
    const IF_EQUAL: u16 = 0x15;
    const RETURN: u16 = 0x06;

    // Bootstrap has already completed. Only the three stdio descriptors
    // survived exec, and the worker needs no additional files or processes.
    const SYSCALLS: &[libc::c_long] = &[
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_close,
        libc::SYS_brk,
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mprotect,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_futex,
        libc::SYS_getrandom,
        libc::SYS_clock_gettime,
        libc::SYS_gettimeofday,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_exit,
        libc::SYS_exit_group,
    ];
    let statement = |code, k| sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let mut filter = vec![
        statement(LOAD_WORD, 4), // seccomp_data.arch
        sock_filter {
            code: IF_EQUAL,
            jt: 1,
            jf: 0,
            k: AUDIT_ARCH,
        },
        statement(RETURN, libc::SECCOMP_RET_KILL_PROCESS),
        statement(LOAD_WORD, 0), // seccomp_data.nr
    ];
    for call in SYSCALLS {
        filter.push(sock_filter {
            code: IF_EQUAL,
            jt: 0,
            jf: 1,
            k: *call as u32,
        });
        filter.push(statement(RETURN, libc::SECCOMP_RET_ALLOW));
    }
    filter.push(statement(
        RETURN,
        libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
    ));
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    // No privilege is needed to stack this filter on the container's policy.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
pub(crate) fn install() -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

pub(crate) fn verify() -> io::Result<()> {
    let denied = [
        std::fs::File::open("/etc/os-release").err(),
        std::net::TcpListener::bind("127.0.0.1:0").err(),
        std::process::Command::new("/bin/true").status().err(),
    ];
    if denied.iter().all(|result| {
        result
            .as_ref()
            .is_some_and(|err| err.kind() == io::ErrorKind::PermissionDenied)
    }) {
        return Ok(());
    }
    Err(io::Error::other(
        "bundled worker sandbox did not confirm external access was denied",
    ))
}
