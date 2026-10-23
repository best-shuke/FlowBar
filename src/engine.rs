use crate::{
    api::{Api, Group},
    config::{self, Settings},
    process::{self, ManagedChild},
    supervisor, update,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Debug)]
pub enum Event {
    Progress(String, String),
    Ready(String, Vec<Group>),
    Stopped(String),
    Failed(String, String),
    Groups(String, Vec<Group>),
    Mode(String, String),
    Selection(String, String, String, Vec<Group>),
    Error(String),
    Update(update::Event),
}
pub type Emit = Arc<dyn Fn(Event) + Send + Sync>;
pub struct Run {
    pub id: String,
    pub settings: Settings,
    pub api: Api,
    pub stage: PathBuf,
    pub status_path: PathBuf,
    pub cancelled: AtomicBool,
    command_pid: AtomicU32,
}
impl Run {
    pub fn log(&self) -> PathBuf {
        for path in [
            self.stage.join("error.log"),
            self.status_path.join("core.log"),
            self.status_path.join("bootstrap.log"),
            self.stage.join("authorization.log"),
        ] {
            if path.metadata().is_ok_and(|m| m.len() > 0) {
                return path;
            }
        }
        self.stage.join("validation.log")
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        // Quit exits right after cancelling, before the worker thread can clean up.
        self.remove_config();
        let _ = config::private_write(&self.stage.join("stop"), &[]);
        let pid = self.command_pid.load(Ordering::SeqCst);
        if pid > 1 {
            process::terminate(pid);
        }
    }
    /// The runtime copy holds node credentials and the controller secret.
    fn remove_config(&self) {
        let _ = fs::remove_file(self.stage.join("config.yaml"));
    }
    /// Runs off the main thread: the first prune after an upgrade can delete many sessions.
    fn prune(&self) {
        let sessions = self.stage.parent().unwrap();
        config::prune_sessions(sessions, |id| {
            let stage = sessions.join(id);
            id == self.id
                || supervisor::running(&stage)
                || supervisor::running(&supervisor::tun_session(&stage, id))
        });
    }
    pub fn group_order(&self) -> Vec<String> {
        fs::read_to_string(config::home().join("profile.yaml"))
            .ok()
            .and_then(|y| config::summary(&y).ok())
            .map(|s| s.groups)
            .unwrap_or_default()
    }
}
pub fn start(settings: Settings, emit: Emit) -> Result<Arc<Run>> {
    settings.validate()?;
    let id = Uuid::new_v4().to_string();
    let stage = config::home().join("Sessions").join(&id);
    config::private_dir(&stage)?;
    let status_path = if settings.tun {
        supervisor::tun_session(&stage, &id)
    } else {
        stage.clone()
    };
    let api = Api {
        port: settings.controller_port,
        secret: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
    };
    let run = Arc::new(Run {
        id,
        settings,
        api,
        stage,
        status_path,
        cancelled: AtomicBool::new(false),
        command_pid: AtomicU32::new(0),
    });
    let background = run.clone();
    thread::spawn(move || {
        background.prune();
        let outcome = worker(&background, &emit);
        background.remove_config();
        match outcome {
            Ok(()) => emit(Event::Stopped(background.id.clone())),
            Err(_)
                if background.cancelled.load(Ordering::SeqCst)
                    && supervisor::read_status(&background.status_path)
                        .is_none_or(|s| s.state != "running") =>
            {
                emit(Event::Stopped(background.id.clone()))
            }
            Err(error) => {
                let _ = config::private_write(&background.stage.join("stop"), &[]);
                let mut message = format!("{error:#}\n");
                for path in [
                    background.status_path.join("core.log"),
                    background.status_path.join("bootstrap.log"),
                    background.stage.join("authorization.log"),
                    background.stage.join("validation.log"),
                ] {
                    if let Ok(contents) = fs::read_to_string(&path) {
                        message.push_str(&format!("\n{}\n{contents}\n", path.display()));
                    }
                }
                let _ =
                    config::private_write(&background.stage.join("error.log"), message.as_bytes());
                emit(Event::Failed(background.id.clone(), error.to_string()));
            }
        }
    });
    Ok(run)
}
fn worker(run: &Run, emit: &Emit) -> Result<()> {
    if run.settings.tun {
        ensure!(
            !verge_running()?,
            "请先退出 Clash Verge，再开启 TUN，避免路由冲突。"
        );
    }
    emit(Event::Progress(run.id.clone(), "正在检查配置…".into()));
    let yaml = fs::read_to_string(config::home().join("profile.yaml"))
        .map_err(|_| anyhow::anyhow!("请先导入完整的 YAML 配置。"))?;
    let runtime = config::runtime(&yaml, &run.settings, &run.api.secret)?;
    let config_file = run.stage.join("config.yaml");
    config::private_write(&config_file, runtime.as_bytes())?;
    let data = config::home().join("Data");
    config::private_dir(&data)?;
    let core = supervisor::core()?;
    ensure!(core.is_file(), "应用中缺少 mihomo 内核，请重新打包。");
    let missing = config::missing_geo(&runtime, &data)?;
    if !missing.is_empty() {
        emit(Event::Progress(
            run.id.clone(),
            format!(
                "缺少地理数据库（{}），配置检查时将尝试下载…",
                missing.join("、")
            ),
        ));
    }
    let mut validate = Command::new(&core);
    process::clean_env(&mut validate)
        .arg("-t")
        .arg("-d")
        .arg(&data)
        .arg("-f")
        .arg(&config_file)
        .current_dir(&data)
        .env("GOGC", "50");
    if let Some(home) = std::env::var_os("HOME") {
        validate.env("HOME", home);
    }
    let log = run.stage.join("validation.log");
    let mut progress = None;
    let output = process::command_logged(
        validate,
        Duration::from_secs(60),
        &run.cancelled,
        &run.command_pid,
        &log,
        |output| {
            if let Some(message) = validation_progress(&String::from_utf8_lossy(output)) {
                if progress.as_ref() != Some(&message) {
                    emit(Event::Progress(run.id.clone(), message.clone()));
                    progress = Some(message);
                }
            }
        },
    )
    .map_err(|error| {
        let contents = fs::read_to_string(&log).unwrap_or_default();
        let message = validation_error(&contents, &data, &format!("配置检查未完成：{error}"));
        error.context(message)
    })?;
    ensure!(
        output.status.success(),
        "{}",
        validation_error(
            &String::from_utf8_lossy(&output.output),
            &data,
            "配置检查失败"
        )
    );
    ensure!(!run.cancelled.load(Ordering::SeqCst), "操作已取消");
    let mut helper = None;
    if run.settings.tun {
        emit(Event::Progress(run.id.clone(), "等待管理员授权…".into()));
        let output = elevate(run)?;
        config::private_write(&run.stage.join("authorization.log"), &output.output)?;
        ensure!(
            output.status.success(),
            "管理员启动失败，请打开日志查看具体原因。"
        );
    } else {
        helper = Some(ManagedChild::new(
            Command::new(std::env::current_exe()?)
                .arg("--user-supervise")
                .arg(std::process::id().to_string())
                .arg(&run.id)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?,
        ));
    }
    emit(Event::Progress(run.id.clone(), "正在启动 mihomo…".into()));
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        ensure!(!run.cancelled.load(Ordering::SeqCst), "操作已取消");
        if let Some(status) = supervisor::read_status(&run.status_path) {
            match status.state.as_str() {
                "running" => break,
                "failed" | "stopped" => bail!("内核未能启动，请打开日志检查端口或配置。"),
                _ => {}
            }
        }
        if let Some(helper) = helper.as_mut() {
            ensure!(
                helper.0.try_wait()?.is_none(),
                "内核监督进程已退出，请查看日志。"
            );
        }
        ensure!(Instant::now() < deadline, "内核启动超时，请查看日志。");
        thread::sleep(Duration::from_millis(100));
    }
    // Both supervisors read the stage copy before reporting running.
    run.remove_config();
    for (group, choice) in &run.settings.selections {
        let _ = run.api.select(group, choice);
    }
    let groups = run.api.groups(&run.group_order()).unwrap_or_default();
    emit(Event::Ready(run.id.clone(), groups));
    loop {
        if run.cancelled.load(Ordering::SeqCst) {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                if supervisor::read_status(&run.status_path).is_some_and(|s| s.state != "running") {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    bail!("内核停止超时，请退出 FlowBar 后检查日志。");
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        if supervisor::read_status(&run.status_path).is_some_and(|s| s.state != "running") {
            bail!("内核已退出，请查看运行日志。");
        }
        if let Some(helper) = helper.as_mut() {
            ensure!(helper.0.try_wait()?.is_none(), "内核监督进程已退出。");
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// mihomo logs the actual resource even when a provider or DNS triggers the download.
fn validation_progress(log: &str) -> Option<String> {
    for line in log.lines().rev() {
        let line = line.to_ascii_lowercase();
        let name = if line.contains("geosite.dat") {
            "GeoSite.dat"
        } else if line.contains("geoip.dat") {
            "GeoIP.dat"
        } else if line.contains("asn.mmdb") {
            "ASN.mmdb"
        } else if line.contains("mmdb") {
            "Country.mmdb / geoip.metadb"
        } else {
            continue;
        };
        if line.contains("start download") || line.contains("invalid, remove and download") {
            return Some(format!("正在下载地理数据库（{name}）…"));
        }
        if line.contains("download") && line.contains("finish") {
            return Some("地理数据库下载完成，正在继续检查配置…".into());
        }
    }
    None
}
fn validation_error(log: &str, data: &Path, fallback: &str) -> String {
    let lower = log.to_ascii_lowercase();
    let geo_error = lower.lines().any(|line| {
        ["mmdb", "geoip", "geosite", "asn.mmdb"]
            .iter()
            .any(|name| line.contains(name))
            && [
                "can't download",
                "can't initial",
                "can't load",
                "can't remove invalid",
            ]
            .iter()
            .any(|marker| line.contains(marker))
    });
    let interrupted_download = fallback.starts_with("配置检查未完成")
        && validation_progress(log).is_some_and(|message| message.starts_with("正在下载"));
    if geo_error || interrupted_download {
        format!(
            "地理数据库加载或下载未能完成。\n\n{fallback}。请确认可访问配置中的 geox-url 下载源，\
             或将所需的 Country.mmdb / geoip.metadb、GeoIP.dat、GeoSite.dat 或 ASN.mmdb 复制到 {} 后重试。\
             文件类型须与配置一致；也可将数据库放在 YAML 同目录后重新导入。详情见 validation.log。",
            data.display()
        )
    } else {
        format!("{fallback}，请打开日志查看原因。")
    }
}

/// Starts the TUN supervisor through the system's administrator authorization, which
/// returns once the core is ready or has failed.
#[cfg(target_os = "macos")]
fn elevate(run: &Run) -> Result<process::Output> {
    let executable = std::env::current_exe()?;
    let command = format!(
        "{} --tun-launch {} {} {}",
        process::shell_quote(executable.to_str().context("应用路径编码无效")?),
        std::process::id(),
        unsafe { libc::getuid() },
        run.id
    );
    let script = format!(
        "do shell script {} with administrator privileges",
        process::apple_string(&command)
    );
    let mut authorization = Command::new("/usr/bin/osascript");
    authorization.args(["-e", &script]);
    process::command(
        authorization,
        Duration::from_secs(180),
        &run.cancelled,
        &run.command_pid,
    )
}
#[cfg(target_os = "linux")]
fn elevate(run: &Run) -> Result<process::Output> {
    let pkexec = ["/usr/bin/pkexec", "/bin/pkexec"]
        .into_iter()
        .map(std::path::Path::new)
        .find(|path| path.is_file())
        .context("TUN 需要通过 polkit 的 pkexec 获取管理员权限，请先安装 polkit。")?;
    let mut authorization = Command::new(pkexec);
    authorization
        .arg(std::env::current_exe()?)
        .arg("--tun-launch")
        .arg(std::process::id().to_string())
        .arg(unsafe { libc::getuid() }.to_string())
        .arg(&run.id);
    process::command(
        authorization,
        Duration::from_secs(180),
        &run.cancelled,
        &run.command_pid,
    )
}
/// Asks UAC to run the launcher elevated. The prompt cannot be cancelled from here, and the
/// launcher's output stays in its bootstrap log.
#[cfg(windows)]
fn elevate(run: &Run) -> Result<process::Output> {
    use std::os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::ExitStatusExt,
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_CANCELLED, WAIT_OBJECT_0},
        System::{
            Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx},
            Threading::{GetExitCodeProcess, GetProcessId, WaitForSingleObject},
        },
        UI::{
            Shell::{
                SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
                ShellExecuteExW,
            },
            WindowsAndMessaging::SW_HIDE,
        },
    };
    let wide = |text: &std::ffi::OsStr| text.encode_wide().chain([0]).collect::<Vec<u16>>();
    let executable = wide(std::env::current_exe()?.as_os_str());
    // Windows paths cannot contain quotes, and a stage path never ends with a backslash.
    let parameters = wide(
        format!(
            "--tun-launch {} \"{}\"",
            std::process::id(),
            run.stage.display()
        )
        .as_ref(),
    );
    let verb = wide("runas".as_ref());
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = executable.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = SW_HIDE;
    let apartment = (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32;
    unsafe { CoInitializeEx(std::ptr::null(), apartment) };
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let error = std::io::Error::last_os_error();
        ensure!(
            error.raw_os_error() != Some(ERROR_CANCELLED as i32),
            "已取消管理员授权。"
        );
        return Err(error).context("无法请求管理员权限");
    }
    ensure!(!info.hProcess.is_null(), "无法跟踪管理员进程");
    let launcher = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    run.command_pid.store(
        unsafe { GetProcessId(launcher.as_raw_handle()) },
        Ordering::SeqCst,
    );
    let deadline = Instant::now() + Duration::from_secs(180);
    let result = (|| {
        while unsafe { WaitForSingleObject(launcher.as_raw_handle(), 100) } != WAIT_OBJECT_0 {
            ensure!(!run.cancelled.load(Ordering::SeqCst), "操作已取消");
            ensure!(Instant::now() < deadline, "操作超时，请查看日志。");
        }
        let mut code = 1;
        unsafe { GetExitCodeProcess(launcher.as_raw_handle(), &mut code) };
        Ok(process::Output {
            status: std::process::ExitStatus::from_raw(code),
            output: format!("管理员启动进程退出代码：{code}\n").into_bytes(),
        })
    })();
    run.command_pid.store(0, Ordering::SeqCst);
    result
}

/// Clash Verge's own TUN would fight over the routes.
#[cfg(target_os = "macos")]
fn verge_running() -> Result<bool> {
    let output = Command::new("/bin/ps").args(["-axo", "comm="]).output()?;
    Ok(String::from_utf8_lossy(&output.stdout).lines().any(|s| {
        s.to_lowercase()
            .contains("/clash verge.app/contents/macos/clash-verge")
    }))
}
#[cfg(target_os = "linux")]
fn verge_running() -> Result<bool> {
    Ok(fs::read_dir("/proc")?.flatten().any(|entry| {
        fs::read_to_string(entry.path().join("comm")).is_ok_and(|name| name.trim() == "clash-verge")
    }))
}
#[cfg(windows)]
fn verge_running() -> Result<bool> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    ensure!(snapshot != INVALID_HANDLE_VALUE, "无法列出进程");
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let handle = std::os::windows::io::AsRawHandle::as_raw_handle(&snapshot);
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut more = unsafe { Process32FirstW(handle, &mut entry) } != 0;
    while more {
        let length = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
        if String::from_utf16_lossy(&entry.szExeFile[..length])
            .eq_ignore_ascii_case("clash-verge.exe")
        {
            return Ok(true);
        }
        more = unsafe { Process32NextW(handle, &mut entry) } != 0;
    }
    Ok(false)
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    #[test]
    fn reports_downloads_and_distinguishes_geo_errors_from_other_config_errors() {
        let pending = r#"level=info msg="Can't find MMDB, start download""#;
        assert!(
            validation_progress(pending)
                .unwrap()
                .contains("Country.mmdb")
        );
        let finished = format!("{pending}\nlevel=info msg=\"Download MMDB finish\"");
        assert!(validation_progress(&finished).unwrap().contains("下载完成"));
        let retry = "GeoSite.dat invalid, remove and download: invalid database";
        assert!(validation_progress(retry).unwrap().contains("GeoSite.dat"));
        let data = Path::new("local-data").join("Data");
        let error = validation_error(
            "can't download MMDB: connection refused",
            &data,
            "配置检查失败",
        );
        assert!(error.contains("geox-url") && error.contains(&data.display().to_string()));
        assert!(error.contains("重新导入"));
        let timeout = validation_error(pending, &data, "配置检查未完成：操作超时");
        assert!(timeout.contains("地理数据库") && timeout.contains("超时"));
        assert_eq!(
            validation_error(
                "rules[0] [GEOIP,CN,missing] error: proxy [missing] not found",
                &data,
                "配置检查失败"
            ),
            "配置检查失败，请打开日志查看原因。"
        );
        assert_eq!(
            validation_error(
                &format!("{finished}\nrules[0] error: proxy [missing] not found"),
                &data,
                "配置检查失败"
            ),
            "配置检查失败，请打开日志查看原因。"
        );
    }
}
