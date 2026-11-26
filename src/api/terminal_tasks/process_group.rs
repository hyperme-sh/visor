use std::{
    io,
    time::{Duration, Instant},
};

pub(super) struct ProcessGroup {
    leader: libc::pid_t,
    interactive: bool,
    stopped: bool,
}

impl ProcessGroup {
    pub(super) fn new(leader: libc::pid_t, interactive: bool) -> Self {
        Self {
            leader,
            interactive,
            stopped: false,
        }
    }

    pub(super) fn leader_exited(&self) -> io::Result<bool> {
        // Keep the leader's PID reserved until cleanup has finished.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.leader as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { info.si_pid() } == self.leader)
    }

    pub(super) fn stop(mut self) -> io::Result<()> {
        self.stopped = true;
        self.terminate()
    }

    fn terminate(&self) -> io::Result<()> {
        match unsafe { libc::kill(-self.leader, libc::SIGKILL) } {
            0 => {}
            _ => {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
        if self.interactive {
            terminate_session(self.leader)?;
        }
        Ok(())
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.terminate();
        }
    }
}

fn terminate_session(session: libc::pid_t) -> io::Result<()> {
    // A job-control shell can leave multiple groups in this session. Rescan
    // while signals are delivered so an in-flight fork is seen before exit.
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut empty = false;
    loop {
        let live = signal_session_members(session)?;
        if live == 0 && empty {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal session did not stop",
            ));
        }
        empty = live == 0;
        std::thread::sleep(Duration::from_millis(15));
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::{
        fs, io,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
    };

    pub(super) fn validate() -> io::Result<()> {
        fs::read_dir("/proc")?;
        let self_fd = open(unsafe { libc::getpid() })?;
        signal(&self_fd, 0).map(|_| ())
    }

    fn open(pid: libc::pid_t) -> io::Result<OwnedFd> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // pidfd_open returns a fresh descriptor with close-on-exec enabled.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
    }

    fn signal(fd: &OwnedFd, value: libc::c_int) -> io::Result<bool> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                value,
                std::ptr::null::<libc::siginfo_t>(),
                0u32,
            )
        };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        Err(error)
    }

    pub(super) fn signal_session_members(session: libc::pid_t) -> io::Result<usize> {
        let mut live = 0;
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|text| text.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            if pid <= 0 || unsafe { libc::getsid(pid) } != session {
                continue;
            }
            let fd = match open(pid) {
                Ok(fd) => fd,
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => continue,
                Err(error) => return Err(error),
            };
            // Recheck membership after acquiring a stable process reference.
            if unsafe { libc::getsid(pid) } != session {
                continue;
            }
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            match unsafe { libc::poll(&mut poll, 1, 0) } {
                0 => live += usize::from(signal(&fd, libc::SIGKILL)?),
                result if result > 0 => {}
                _ => return Err(io::Error::last_os_error()),
            }
        }
        Ok(live)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::{io, mem::size_of, ptr};

    pub(super) fn validate() -> io::Result<()> {
        list_pids().map(|_| ())
    }

    fn list_pids() -> io::Result<Vec<libc::pid_t>> {
        let count = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
        if count <= 0 {
            return Err(io::Error::last_os_error());
        }
        let mut pids = vec![0; count as usize + 64];
        let bytes = libc::c_int::try_from(pids.len() * size_of::<libc::pid_t>())
            .map_err(|_| io::Error::other("process table exceeds its limit"))?;
        let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if filled <= 0 {
            return Err(io::Error::last_os_error());
        }
        if filled as usize >= pids.len() {
            return Err(io::Error::other(
                "process table changed while reading terminal members",
            ));
        }
        pids.truncate(filled as usize);
        Ok(pids)
    }

    pub(super) fn signal_session_members(session: libc::pid_t) -> io::Result<usize> {
        let mut live = 0;
        for pid in list_pids()? {
            if pid <= 0 || unsafe { libc::getsid(pid) } != session {
                continue;
            }
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
            let result = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    (&mut info as *mut libc::proc_bsdinfo).cast(),
                    size,
                )
            };
            if result != size {
                if unsafe { libc::getsid(pid) } != session {
                    continue;
                }
                return Err(io::Error::last_os_error());
            }
            if info.pbi_status == libc::SZOMB || unsafe { libc::getsid(pid) } != session {
                continue;
            }
            if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
                live += 1;
            } else {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
        Ok(live)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use std::io;

    pub(super) fn validate() -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "terminal session cleanup unavailable",
        ))
    }
    pub(super) fn signal_session_members(_: libc::pid_t) -> io::Result<usize> {
        validate().map(|()| 0)
    }
}

use platform::signal_session_members;

pub(super) fn validate_interactive() -> io::Result<()> {
    platform::validate()
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::api::terminal_tasks::interactive::Master;
    use std::io::Write;

    const FIXTURE: &str = "VISOR_TEST_TERMINAL_JOB_CONTROL";

    #[test]
    fn job_control_fixture() {
        if std::env::var_os(FIXTURE).is_none() {
            return;
        }
        // Fork only inside the isolated fixture. The child uses only
        // async-signal-safe system calls.
        let mut ready = [0; 2];
        assert_eq!(unsafe { libc::pipe(ready.as_mut_ptr()) }, 0);
        let job = unsafe { libc::fork() };
        assert!(job >= 0);
        if job == 0 {
            unsafe {
                libc::close(ready[0]);
                // Bound the fixture independently of the implementation.
                libc::alarm(5);
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                if libc::setpgid(0, 0) != 0 || libc::write(ready[1], b"x".as_ptr().cast(), 1) != 1 {
                    libc::_exit(1);
                }
                libc::close(ready[1]);
                loop {
                    libc::pause();
                }
            }
        }
        let mut marker = 0u8;
        unsafe { libc::close(ready[1]) };
        assert_eq!(
            unsafe { libc::read(ready[0], (&mut marker as *mut u8).cast(), 1) },
            1
        );
        unsafe { libc::close(ready[0]) };
        println!("terminal-job={job}");
        std::io::stdout().flush().unwrap();
        unsafe { libc::waitpid(job, std::ptr::null_mut(), 0) };
    }

    #[tokio::test]
    async fn stopping_the_terminal_session_closes_a_separate_job_control_group() {
        validate_interactive().unwrap();
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "api::terminal_tasks::process_group::tests::job_control_fixture",
                "--nocapture",
            ])
            .env(FIXTURE, "1")
            .kill_on_drop(true);
        let master = Master::attach(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        drop(command);
        let leader = child.id().unwrap() as libc::pid_t;
        let group = ProcessGroup::new(leader, true);

        let job = tokio::time::timeout(Duration::from_secs(3), async {
            let mut bytes = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let count = master.read(&mut buffer).await.unwrap();
                assert!(
                    count > 0,
                    "job-control fixture exited before reporting its group"
                );
                bytes.extend_from_slice(&buffer[..count]);
                let text = String::from_utf8_lossy(&bytes);
                if let Some((_, output)) = text.split_once("terminal-job=") {
                    if let Some((value, _)) = output.split_once('\n') {
                        break value.trim().parse::<libc::pid_t>().unwrap();
                    }
                }
            }
        })
        .await
        .unwrap();
        assert_ne!(job, leader);
        assert_eq!(unsafe { libc::getpgid(job) }, job);
        assert_eq!(unsafe { libc::getsid(job) }, leader);

        tokio::task::spawn_blocking(move || group.stop())
            .await
            .unwrap()
            .unwrap();
        // The separate group inherits the terminal's slave. EOF proves it
        // released the descriptor despite ignoring SIGHUP.
        tokio::time::timeout(Duration::from_secs(1), async {
            while master.read(&mut [0; 1024]).await.unwrap() != 0 {}
        })
        .await
        .unwrap();
        child.wait().await.unwrap();
    }
}
