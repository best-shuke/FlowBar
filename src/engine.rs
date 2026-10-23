use crate::{
    api::{Api, Group},
    config::{self, Settings},
    process::{self, ManagedChild},
    supervisor, update,
};
use anyhow::{Result, bail, ensure};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, Ordering},
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
    command_pid: AtomicI32,
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
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
    /// The runtime copy holds node credentials and the controller secret.
    fn remove_config(&self) {
        let _ = fs::remove_file(self.stage.join("config.yaml"));
    }
    /// Runs off the main thread: the first prune after an upgrade can delete many sessions.
    fn prune(&self) {
        let uid = unsafe { libc::getuid() };
        let sessions = self.stage.parent().unwrap();
        config::prune_sessions(sessions, |id| {
            id == self.id
                || supervisor::running(&sessions.join(id))
                || supervisor::running(&supervisor::root_session(uid, id))
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
        supervisor::root_session(unsafe { libc::getuid() }, &id)
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
        command_pid: AtomicI32::new(0),
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
        let output = Command::new("/bin/ps").args(["-axo", "comm="]).output()?;
        let names = String::from_utf8_lossy(&output.stdout);
        ensure!(
            !names.lines().any(|s| s
                .to_lowercase()
                .contains("/clash verge.app/contents/macos/clash-verge")),
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
    let core = supervisor::resources()?.join("mihomo");
    ensure!(core.is_file(), "应用中缺少 mihomo 内核，请重新打包。");
    let mut validate = Command::new(&core);
    validate
        .arg("-t")
        .arg("-d")
        .arg(&data)
        .arg("-f")
        .arg(&config_file)
        .current_dir(&data)
        .env_clear()
        .env("HOME", std::env::var_os("HOME").unwrap_or_default())
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("GOGC", "50");
    let output = process::command(
        validate,
        Duration::from_secs(60),
        &run.cancelled,
        &run.command_pid,
    )?;
    config::private_write(&run.stage.join("validation.log"), &output.output)?;
    ensure!(
        output.status.success(),
        "配置检查失败，请打开日志查看原因。"
    );
    ensure!(!run.cancelled.load(Ordering::SeqCst), "操作已取消");
    let mut helper = None;
    if run.settings.tun {
        emit(Event::Progress(run.id.clone(), "等待管理员授权…".into()));
        let executable = std::env::current_exe()?;
        let command = format!(
            "{} --tun-launch {} {} {}",
            process::shell_quote(
                executable
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("应用路径编码无效"))?
            ),
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
        let output = process::command(
            authorization,
            Duration::from_secs(180),
            &run.cancelled,
            &run.command_pid,
        )?;
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
