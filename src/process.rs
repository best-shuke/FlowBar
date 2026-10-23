use anyhow::{Context, Result, ensure};
#[cfg(unix)]
use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
#[cfg(windows)]
use std::os::windows::{
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::CommandExt,
};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
        Threading::{
            CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, OpenProcess, PROCESS_SYNCHRONIZE,
            PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
        },
    },
};

pub struct Output {
    pub status: ExitStatus,
    pub output: Vec<u8>,
}
/// Runs a short command without a persistent log, for the Unix administrator launch.
#[cfg(any(unix, test))]
pub fn command(
    command: Command,
    timeout: Duration,
    cancelled: &AtomicBool,
    pid: &AtomicU32,
) -> Result<Output> {
    let path = std::env::temp_dir().join(format!("flowbar-{}.log", uuid::Uuid::new_v4()));
    let result = command_logged(command, timeout, cancelled, pid, &path, |_| {});
    let _ = fs::remove_file(path);
    result
}
/// Streams output to a private, persistent log, including on timeout or cancellation.
/// `progress` sees the last 64 KiB when the log changes, on the calling worker thread.
pub fn command_logged(
    mut command: Command,
    timeout: Duration,
    cancelled: &AtomicBool,
    pid: &AtomicU32,
    path: &Path,
    mut progress: impl FnMut(&[u8]),
) -> Result<Output> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let log = options.open(path)?;
    // A cloned file shares its offset with the child. Read through a separate handle
    // so polling never moves the child's write position.
    let mut reader = fs::File::open(path)?;
    let result = (|| {
        ensure!(!cancelled.load(Ordering::SeqCst), "操作已取消");
        background(&mut command)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log.try_clone()?));
        let mut child = ManagedChild::new(command.spawn()?);
        pid.store(child.0.id(), Ordering::SeqCst);
        let deadline = Instant::now() + timeout;
        let mut length = 0;
        let mut output = Vec::new();
        let status = loop {
            let status = child.0.try_wait()?;
            let current = reader.metadata()?.len();
            if current != length || status.is_some() {
                reader.seek(SeekFrom::Start(current.saturating_sub(65536)))?;
                output.clear();
                (&mut reader).take(65536).read_to_end(&mut output)?;
                progress(&output);
                length = current;
            }
            ensure!(!cancelled.load(Ordering::SeqCst), "操作已取消");
            if let Some(status) = status {
                break status;
            }
            ensure!(Instant::now() < deadline, "操作超时，请查看日志。");
            thread::sleep(Duration::from_millis(50));
        };
        Ok(Output { status, output })
    })();
    pid.store(0, Ordering::SeqCst);
    result
}
#[cfg(any(target_os = "macos", all(test, unix)))]
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
#[cfg(target_os = "macos")]
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

/// Keeps console programs such as mihomo and curl from opening a console window on Windows.
pub fn background(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}
/// A child environment without the user's proxy variables or other settings. HOME and
/// program-specific variables are left to the caller.
pub fn clean_env(command: &mut Command) -> &mut Command {
    command.env_clear();
    #[cfg(unix)]
    command.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    // Winsock and the TLS stack fail to start without the system directories.
    #[cfg(windows)]
    for name in [
        "SystemRoot",
        "SystemDrive",
        "windir",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "ProgramData",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

/// Starts a separate session without relying on a terminal or shell job control.
#[cfg(unix)]
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
/// Starts a process that outlives this one and does not share its console.
#[cfg(windows)]
pub fn spawn_detached(command: &mut Command) -> Result<ManagedChild> {
    command
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    Ok(ManagedChild::new(command.spawn()?))
}

/// Whether a process exists. Unlike a child handle, this works for any process ID.
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    // Zero and negative IDs would address process groups.
    (1..=i32::MAX as u32).contains(&pid)
        && (unsafe { libc::kill(pid as i32, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}
#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    open(pid, PROCESS_SYNCHRONIZE)
        .is_some_and(|process| unsafe { WaitForSingleObject(raw(&process), 0) } == WAIT_TIMEOUT)
}
/// Asks a process to exit; on Windows, where there is no such request, ends it.
pub fn terminate(pid: u32) {
    #[cfg(unix)]
    if (1..=i32::MAX as u32).contains(&pid) {
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
    #[cfg(windows)]
    if let Some(process) = open(pid, PROCESS_TERMINATE) {
        unsafe { TerminateProcess(raw(&process), 1) };
    }
}
/// Waits up to `timeout` for a process that is not our child to exit.
#[cfg(any(not(target_os = "macos"), test))]
pub fn wait_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
    true
}
#[cfg(windows)]
fn open(pid: u32, access: u32) -> Option<OwnedHandle> {
    let handle = unsafe { OpenProcess(access, 0, pid) };
    (!handle.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(handle) })
}
#[cfg(windows)]
fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
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
        // Windows has no termination request, so the process is ended right away.
        #[cfg(unix)]
        {
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

#[cfg(target_os = "macos")]
struct ParentWatch(std::os::fd::OwnedFd);
#[cfg(target_os = "macos")]
impl ParentWatch {
    fn new(parent: u32) -> Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
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
    /// Waits up to a second; true once the parent has exited.
    fn parent_exited(&self) -> Result<bool> {
        use std::os::fd::AsRawFd;
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
            anyhow::bail!("进程监听失败");
        }
        Ok(count > 0)
    }
}

/// A pidfd reports the parent's exit even after its PID is reused. Kernels before 5.3
/// lack it, so those poll the PID instead.
#[cfg(target_os = "linux")]
struct ParentWatch(Option<std::os::fd::OwnedFd>, u32);
#[cfg(target_os = "linux")]
impl ParentWatch {
    fn new(parent: u32) -> Result<Self> {
        use std::os::fd::{FromRawFd, OwnedFd};
        ensure!(parent > 1, "父进程无效");
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, parent as libc::pid_t, 0) };
        if fd >= 0 {
            return Ok(Self(
                Some(unsafe { OwnedFd::from_raw_fd(fd as i32) }),
                parent,
            ));
        }
        let missing = std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        ensure!(!missing && alive(parent), "主程序已经退出或无法监听");
        Ok(Self(None, parent))
    }
    /// Waits up to a second; true once the parent has exited.
    fn parent_exited(&self) -> Result<bool> {
        use std::os::fd::AsRawFd;
        let Some(fd) = &self.0 else {
            thread::sleep(Duration::from_secs(1));
            return Ok(!alive(self.1));
        };
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let count = unsafe { libc::poll(&mut poll, 1, 1000) };
        if count < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            anyhow::bail!("进程监听失败");
        }
        Ok(count > 0)
    }
}

#[cfg(windows)]
struct ParentWatch(OwnedHandle);
#[cfg(windows)]
impl ParentWatch {
    fn new(parent: u32) -> Result<Self> {
        ensure!(parent > 0, "父进程无效");
        let process = open(parent, PROCESS_SYNCHRONIZE).context("主程序已经退出或无法监听")?;
        Ok(Self(process))
    }
    /// Waits up to a second; true once the parent has exited.
    fn parent_exited(&self) -> Result<bool> {
        Ok(unsafe { WaitForSingleObject(raw(&self.0), 1000) } == WAIT_OBJECT_0)
    }
}

/// Ends the core together with its supervisor, even when the supervisor itself is killed.
#[cfg(windows)]
fn kill_on_close(child: &Child) -> Result<OwnedHandle> {
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    ensure!(!job.is_null(), "无法创建进程作业");
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    ensure!(
        unsafe {
            SetInformationJobObject(
                raw(&job),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } != 0
            && unsafe { AssignProcessToJobObject(raw(&job), child.as_raw_handle()) } != 0,
        "无法设置进程作业"
    );
    Ok(job)
}

#[cfg(unix)]
struct Signals(signal_hook::SigId, signal_hook::SigId);
#[cfg(unix)]
impl Drop for Signals {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.0);
        signal_hook::low_level::unregister(self.1);
    }
}

pub fn supervise(
    mut child_command: Command,
    parent: u32,
    stop_file: &Path,
    log: Option<&fs::File>,
    on_started: impl FnOnce(u32, &AtomicBool) -> Result<()>,
) -> Result<ExitStatus> {
    let watch = ParentWatch::new(parent)?;
    ensure!(!stop_file.exists(), "连接已取消");
    let signalled = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    let _signals = Signals(
        signal_hook::flag::register(libc::SIGTERM, signalled.clone())?,
        signal_hook::flag::register(libc::SIGINT, signalled.clone())?,
    );
    // The core also stops if this supervisor is killed.
    #[cfg(target_os = "linux")]
    unsafe {
        child_command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    background(&mut child_command);
    let mut child = ManagedChild::new(child_command.spawn().context("无法启动 mihomo")?);
    // Best effort: a job that forbids nesting cannot take the core.
    #[cfg(windows)]
    let _job = kill_on_close(&child.0).ok();
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
    use anyhow::bail;
    fn sleeper() -> Command {
        #[cfg(unix)]
        let mut c = Command::new("/bin/sleep");
        #[cfg(unix)]
        c.arg("30");
        #[cfg(windows)]
        let mut c = Command::new("ping");
        #[cfg(windows)]
        c.args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null());
        c
    }
    #[cfg(unix)]
    #[test]
    fn quoting() {
        let input = "spaces ' double \" $(printf unsafe) `id` \n尾";
        let mut c = Command::new("/bin/sh");
        c.args(["-c", &format!("printf %s {}", shell_quote(input))]);
        let output = command(
            c,
            Duration::from_secs(2),
            &AtomicBool::new(false),
            &AtomicU32::new(0),
        )
        .unwrap();
        assert_eq!(String::from_utf8(output.output).unwrap(), input);
    }
    #[test]
    fn timeout() {
        let start = Instant::now();
        assert!(
            command(
                sleeper(),
                Duration::from_millis(50),
                &AtomicBool::new(false),
                &AtomicU32::new(0)
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(3));
    }
    fn logging_sleeper() -> Command {
        #[cfg(unix)]
        {
            let mut command = Command::new("/bin/sh");
            command.args([
                "-c",
                "printf 'validation-started\\n'; printf 'download-pending\\n' >&2; exec /bin/sleep 30",
            ]);
            command
        }
        #[cfg(windows)]
        {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoProfile", "-NonInteractive", "-Command",
                "[Console]::Out.WriteLine('validation-started'); [Console]::Error.WriteLine('download-pending'); Start-Sleep -Seconds 30",
            ]);
            command
        }
    }
    #[test]
    fn logged_command_reports_live_output_and_keeps_it_on_cancel_or_timeout() {
        // The output must arrive before the timeout, and PowerShell can take seconds to
        // start on a cold CI runner.
        let timeout = Duration::from_secs(if cfg!(windows) { 20 } else { 3 });
        for cancel in [false, true] {
            let temporary = tempfile::tempdir().unwrap();
            let path = temporary.path().join("validation.log");
            let cancelled = AtomicBool::new(false);
            let pid = AtomicU32::new(0);
            let mut observed = 0;
            let result = command_logged(
                logging_sleeper(),
                timeout,
                &cancelled,
                &pid,
                &path,
                |output| {
                    let text = String::from_utf8_lossy(output);
                    if text.contains("validation-started") && text.contains("download-pending") {
                        observed = pid.load(Ordering::SeqCst);
                        // The callback must run while the command is still running.
                        assert!(alive(observed));
                        if cancel {
                            cancelled.store(true, Ordering::SeqCst);
                        }
                    }
                },
            );
            assert!(result.err().unwrap().to_string().contains(if cancel {
                "取消"
            } else {
                "超时"
            }));
            assert!(observed > 1 && !alive(observed));
            assert_eq!(pid.load(Ordering::SeqCst), 0);
            let log = fs::read_to_string(&path).unwrap();
            assert!(log.contains("validation-started") && log.contains("download-pending"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }
    #[test]
    fn parent_exit_stops_child() {
        let mut parent = ManagedChild::new(sleeper().spawn().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let parent_id = parent.0.id();
        let mut child_id = 0;
        supervise(
            sleeper(),
            parent_id,
            &tmp.path().join("stop"),
            None,
            |pid, _| {
                child_id = pid;
                parent.0.kill()?;
                Ok(())
            },
        )
        .unwrap();
        assert!(!alive(child_id));
        let _ = parent.0.wait();
    }
    #[test]
    fn stop_request_and_failed_startup_reap_child() {
        for fail in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let stop = tmp.path().join("stop");
            let mut child_id = 0;
            let result = supervise(sleeper(), std::process::id(), &stop, None, |pid, _| {
                child_id = pid;
                if fail {
                    bail!("startup failed")
                }
                fs::write(&stop, [])?;
                Ok(())
            });
            assert_eq!(result.is_err(), fail);
            assert!(!alive(child_id));
        }
    }
    #[test]
    fn terminate_and_wait_for_unrelated_process() {
        let mut child = sleeper().spawn().unwrap();
        let pid = child.id();
        assert!(alive(pid) && !wait_exit(pid, Duration::from_millis(100)));
        terminate(pid);
        let _ = child.wait();
        assert!(!alive(pid) && wait_exit(pid, Duration::from_millis(100)));
    }

    #[test]
    fn detached_process_has_own_session_and_reports_spawn_errors() {
        let mut command = sleeper();
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let child = spawn_detached(&mut command).unwrap();
        let pid = child.0.id();
        #[cfg(unix)]
        {
            assert_eq!(unsafe { libc::getsid(pid as i32) }, pid as i32);
            assert_ne!(unsafe { libc::getsid(0) }, pid as i32);
        }
        drop(child);
        assert!(!alive(pid));
        assert!(spawn_detached(&mut Command::new("/flowbar-missing-executable")).is_err());
    }
}
