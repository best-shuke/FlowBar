use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::OpenOptionsExt, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

pub struct Output {
    pub status: ExitStatus,
    pub output: Vec<u8>,
}
pub fn command(
    mut command: Command,
    timeout: Duration,
    cancelled: &AtomicBool,
    pid: &AtomicI32,
) -> Result<Output> {
    let path = std::env::temp_dir().join(format!("flowbar-{}.log", Uuid::new_v4()));
    let mut log = fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    let result = (|| {
        command
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log.try_clone()?));
        let mut child = ManagedChild::new(command.spawn()?);
        pid.store(child.0.id() as i32, Ordering::SeqCst);
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            ensure!(!cancelled.load(Ordering::SeqCst), "操作已取消");
            ensure!(Instant::now() < deadline, "操作超时，请查看日志。");
            thread::sleep(Duration::from_millis(50));
        };
        let length = log.metadata()?.len();
        log.seek(SeekFrom::Start(length.saturating_sub(65536)))?;
        let mut output = Vec::new();
        log.read_to_end(&mut output)?;
        Ok(Output { status, output })
    })();
    pid.store(0, Ordering::SeqCst);
    let _ = fs::remove_file(path);
    result
}
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
pub fn apple_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

/// Starts a separate session without relying on a terminal or shell job control.
pub fn spawn_detached(command: &mut Command) -> Result<ManagedChild> {
    command.stdin(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(ManagedChild::new(command.spawn()?))
}

pub struct ManagedChild(pub Child, bool);
impl ManagedChild {
    pub fn new(child: Child) -> Self {
        Self(child, true)
    }
    /// Transfer lifetime management to the supervisor's parent/stop-file watch.
    pub fn detach(mut self) {
        self.1 = false;
    }
    pub fn stop(&mut self) {
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.1 {
            self.stop();
        }
    }
}

struct ParentWatch(OwnedFd);
impl ParentWatch {
    fn new(parent: i32) -> Result<Self> {
        ensure!(parent > 1, "父进程无效");
        let fd = unsafe { libc::kqueue() };
        ensure!(fd >= 0, "无法监听主程序退出");
        let watch = Self(unsafe { OwnedFd::from_raw_fd(fd) });
        let event = libc::kevent {
            ident: parent as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let result = unsafe {
            libc::kevent(
                watch.0.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        ensure!(result == 0, "主程序已经退出或无法监听");
        Ok(watch)
    }
    fn parent_exited(&self) -> Result<bool> {
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        let timeout = libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        let count = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                std::ptr::null(),
                0,
                &mut event,
                1,
                &timeout,
            )
        };
        if count < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            bail!("进程监听失败");
        }
        Ok(count > 0)
    }
}

pub fn supervise(
    mut child_command: Command,
    parent: i32,
    stop_file: &Path,
    log: Option<&fs::File>,
    on_started: impl FnOnce(u32, &AtomicBool) -> Result<()>,
) -> Result<ExitStatus> {
    let watch = ParentWatch::new(parent)?;
    ensure!(!stop_file.exists(), "连接已取消");
    let signalled = Arc::new(AtomicBool::new(false));
    let term = signal_hook::flag::register(libc::SIGTERM, signalled.clone())?;
    let interrupt = signal_hook::flag::register(libc::SIGINT, signalled.clone())?;
    struct Signals(signal_hook::SigId, signal_hook::SigId);
    impl Drop for Signals {
        fn drop(&mut self) {
            signal_hook::low_level::unregister(self.0);
            signal_hook::low_level::unregister(self.1);
        }
    }
    let _signals = Signals(term, interrupt);
    let mut child = ManagedChild::new(child_command.spawn().context("无法启动 mihomo")?);
    on_started(child.0.id(), &signalled)?;
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        if stop_file.exists() || signalled.load(Ordering::SeqCst) || watch.parent_exited()? {
            child.stop();
            return Ok(child.0.wait()?);
        }
        if let Some(mut log) = log {
            if log.metadata()?.len() > 2 * 1024 * 1024 {
                log.set_len(0)?;
                log.seek(SeekFrom::Start(0))?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sleeper() -> Command {
        let mut c = Command::new("/bin/sleep");
        c.arg("30");
        c
    }
    #[test]
    fn quoting_and_timeout() {
        let input = "spaces ' double \" $(printf unsafe) `id` \n尾";
        let mut c = Command::new("/bin/sh");
        c.args(["-c", &format!("printf %s {}", shell_quote(input))]);
        let output = command(
            c,
            Duration::from_secs(2),
            &AtomicBool::new(false),
            &AtomicI32::new(0),
        )
        .unwrap();
        assert_eq!(String::from_utf8(output.output).unwrap(), input);
        let start = Instant::now();
        assert!(
            command(
                sleeper(),
                Duration::from_millis(50),
                &AtomicBool::new(false),
                &AtomicI32::new(0)
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(3));
    }
    #[test]
    fn parent_exit_stops_child() {
        let mut parent = ManagedChild::new(sleeper().spawn().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let mut child_id = 0;
        supervise(
            sleeper(),
            parent.0.id() as i32,
            &tmp.path().join("stop"),
            None,
            |pid, _| {
                child_id = pid;
                unsafe {
                    libc::kill(parent.0.id() as i32, libc::SIGTERM);
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(unsafe { libc::kill(child_id as i32, 0) }, -1);
        let _ = parent.0.wait();
    }
    #[test]
    fn stop_request_and_failed_startup_reap_child() {
        for fail in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let stop = tmp.path().join("stop");
            let mut child_id = 0;
            let result = supervise(
                sleeper(),
                std::process::id() as i32,
                &stop,
                None,
                |pid, _| {
                    child_id = pid;
                    if fail {
                        bail!("startup failed")
                    }
                    fs::write(&stop, [])?;
                    Ok(())
                },
            );
            assert_eq!(result.is_err(), fail);
            assert_eq!(unsafe { libc::kill(child_id as i32, 0) }, -1);
        }
    }

    #[test]
    fn detached_process_has_own_session_and_reports_spawn_errors() {
        let mut command = sleeper();
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let child = spawn_detached(&mut command).unwrap();
        let pid = child.0.id() as i32;
        assert_eq!(unsafe { libc::getsid(pid) }, pid);
        assert_ne!(unsafe { libc::getsid(0) }, pid);
        drop(child);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert!(spawn_detached(&mut Command::new("/flowbar-missing-executable")).is_err());
    }
}
