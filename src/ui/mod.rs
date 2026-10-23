//! State and behaviour shared by the native front ends: an AppKit menu bar app on macOS,
//! a Win32 tray app on Windows and a GTK app with a StatusNotifierItem on Linux. Each front
//! end implements [`Shell`] and forwards its menu and window actions to [`App`].

#[cfg(not(target_os = "macos"))]
mod icon;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(target_os = "macos")]
use macos as sys;
#[cfg(windows)]
use windows as sys;

use crate::{
    api::Group,
    config::{self, Settings},
    engine::{self, Emit, Event, Run},
    update::{self, Release},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, SystemTime},
};

pub use sys::run;

const UPDATE_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// Arguments the interface itself understands.
pub const ARGUMENTS: &[&str] = &[
    "--show-settings",
    "--snapshot",
    "--no-autoconnect",
    "--ui-smoke",
    "--autostart",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}
pub enum Update {
    Idle,
    Unavailable,
    Checking,
    Available(Release),
    Downloading(Release),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Action {
    Connect,
    Settings,
    Import,
    Reveal,
    Quit,
    Tun,
    Auto,
    Login,
    Logs,
    Data,
    CheckUpdate,
    InstallUpdate,
    AutoUpdate,
    Mode(String),
    Select(String, String),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Login {
    Disabled,
    Enabled,
    /// macOS only: registered, but waiting for approval in System Settings.
    RequiresApproval,
}
pub fn login_status() -> Login {
    sys::login_status()
}

/// A status menu entry. Items with `checked: None` are plain commands; the others show
/// whether an option or a choice is selected.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Item {
        title: String,
        action: Option<Action>,
        enabled: bool,
        checked: Option<bool>,
    },
    Menu {
        title: String,
        entries: Vec<Entry>,
        /// The 策略组与节点 submenu, which a front end may refresh while it is open.
        #[cfg_attr(target_os = "linux", allow(dead_code))]
        groups: bool,
    },
    Separator,
}
fn item(title: &str, action: Option<Action>, enabled: bool, checked: Option<bool>) -> Entry {
    Entry::Item {
        title: title.into(),
        action,
        enabled,
        checked,
    }
}

/// What the settings window shows.
pub struct View {
    pub imported: bool,
    pub profile: String,
    pub stats: String,
    /// Import, TUN and the ports can change only while stopped.
    pub editable: bool,
    pub tun: bool,
    pub tun_hint: &'static str,
    pub ports: [u16; 3],
    #[cfg_attr(windows, allow(dead_code))]
    pub ports_hint: Option<&'static str>,
    pub login: Login,
    pub auto_connect: bool,
    pub login_hint: &'static str,
    pub auto_update: bool,
    pub update_title: String,
    pub update_enabled: bool,
    pub phase: Phase,
    pub message: String,
    pub logs: bool,
    pub start_title: &'static str,
    pub start_enabled: bool,
}
/// Values entered in the settings window.
pub struct Form {
    pub tun: bool,
    pub auto_connect: bool,
    pub auto_update: bool,
    pub ports: Vec<String>,
}

/// What a front end provides. Methods run on the UI thread and must not hold a borrow of
/// the front end's own state while calling back into [`App`], which may show a modal alert.
pub trait Shell {
    /// Shows the state in the status item and, when open, in the settings window.
    fn render(&self);
    /// Opens the settings window if needed and brings it to the front.
    fn show_settings(&self);
    fn close_settings(&self);
    fn settings_open(&self) -> bool;
    /// The settings window's values, while it is open.
    fn form(&self) -> Option<Form>;
    fn choose_profile(&self) -> Option<PathBuf>;
    /// Shows a modal message and returns the index of the clicked button; without
    /// buttons it offers a single OK. A dismissed alert returns `buttons.len()`.
    fn alert(&self, title: &str, message: &str, buttons: &[&str]) -> usize;
    /// The groups changed while the status menu may be open; the status item's menu
    /// shows them, but the settings window does not.
    fn groups_changed(&self) {}
    /// Sends [`update::Event::Due`] after ten seconds and then hourly.
    fn schedule_updates(&self);
    fn snapshot(&self, _path: &Path) {}
    fn quit(&self);
}

pub struct Ui {
    pub settings: Settings,
    pub phase: Phase,
    pub message: String,
    pub groups: Vec<Group>,
    pub run: Option<Arc<Run>>,
    log: Option<PathBuf>,
    /// Profile counts, parsed when the settings window opens and after an import, not on every render.
    summary: Option<String>,
    refreshing: bool,
    pub operation: bool,
    update: Update,
    /// Version an automatic check has already offered in this launch.
    announced: Option<String>,
    checked: Option<SystemTime>,
    smoke_step: Option<u8>,
}
impl Default for Ui {
    fn default() -> Self {
        Self {
            settings: Settings::load(),
            phase: Phase::Stopped,
            message: "已停止".into(),
            groups: vec![],
            run: None,
            log: None,
            summary: None,
            refreshing: false,
            operation: false,
            update: Update::Idle,
            announced: None,
            checked: None,
            smoke_step: std::env::args().any(|a| a == "--ui-smoke").then_some(0),
        }
    }
}

pub struct App {
    pub ui: RefCell<Ui>,
    receiver: mpsc::Receiver<Event>,
    pub emit: Emit,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    committing: Cell<bool>,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    reset_ports: Cell<bool>,
}
impl App {
    /// `wake` runs on the sending thread and must schedule [`App::drain`] on the UI thread.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (sender, receiver) = mpsc::channel();
        let emit: Emit = Arc::new(move |event| {
            if sender.send(event).is_ok() {
                wake();
            }
        });
        Self {
            ui: RefCell::new(Ui::default()),
            receiver,
            emit,
            committing: Cell::new(false),
            reset_ports: Cell::new(false),
        }
    }
    /// Runs once the front end has created its status item.
    pub fn launch(&self, shell: &dyn Shell) {
        shell.render();
        let arguments: Vec<_> = std::env::args().collect();
        if arguments.iter().any(|a| a == "--ui-smoke") {
            shell.show_settings();
            let check = (|| -> Result<()> {
                if std::env::var_os("FLOWBAR_TEST_LOGIN").is_some() {
                    login_roundtrip()?;
                }
                self.act(shell, Action::Auto)?;
                self.act(shell, Action::Auto)?;
                self.act(shell, Action::Tun)?;
                self.act(shell, Action::Tun)?;
                ensure!(!self.ui.borrow().settings.tun, "UI 测试必须保持 TUN 关闭");
                self.connect(shell)
            })();
            if let Err(error) = check {
                eprintln!("FAIL Rust UI smoke: {error:#}");
                std::process::exit(1);
            }
            return;
        }
        let snapshot = arguments.iter().position(|a| a == "--snapshot");
        if snapshot.is_some()
            || arguments.iter().any(|a| a == "--show-settings")
            || !config::home().join("profile.yaml").exists()
        {
            shell.show_settings();
        }
        if let Some(path) = snapshot.and_then(|index| arguments.get(index + 1)) {
            shell.snapshot(Path::new(path));
        }
        if snapshot.is_none() {
            shell.schedule_updates();
        }
        let auto_connect = self.ui.borrow().settings.auto_connect;
        if auto_connect && !arguments.iter().any(|a| a == "--no-autoconnect") {
            if let Err(error) = self.connect(shell) {
                self.error(shell, &error.to_string());
            }
        }
    }
    /// Stops the connection when the app quits.
    pub fn stop(&self) {
        if let Some(run) = &self.ui.borrow().run {
            run.cancel();
        }
    }
    pub fn stopped(&self) -> bool {
        matches!(self.ui.borrow().phase, Phase::Stopped | Phase::Failed)
    }
    /// Menu actions are available while no connection change or menu operation is pending.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub fn menu_enabled(&self) -> bool {
        let ui = self.ui.borrow();
        !matches!(ui.phase, Phase::Starting | Phase::Stopping) && !ui.operation
    }
    /// Reloads the profile counts shown in a newly opened settings window.
    pub fn load_summary(&self) {
        self.ui.borrow_mut().summary = profile_summary();
    }
    /// Whether the settings window should overwrite its port fields even while one is being
    /// edited, after a failed save. Only that window's render may take it.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn take_reset_ports(&self) -> bool {
        self.reset_ports.take()
    }

    /// Runs an action and shows its error, if any.
    pub fn perform(&self, shell: &dyn Shell, action: Action) {
        if let Err(error) = self.act(shell, action) {
            self.error(shell, &error.to_string());
            shell.render();
        }
    }
    pub fn act(&self, shell: &dyn Shell, action: Action) -> Result<()> {
        match action {
            Action::Connect => self.connect(shell)?,
            Action::Settings => shell.show_settings(),
            Action::Import => self.import(shell)?,
            Action::Reveal => sys::open(&config::home().join("profile.yaml"), true),
            Action::Quit => shell.quit(),
            Action::Login => sys::toggle_login()?,
            Action::Logs => {
                let ui = self.ui.borrow();
                let path = ui.run.as_ref().map(|r| r.log()).or_else(|| ui.log.clone());
                if let Some(path) = path {
                    if path.exists() {
                        sys::open(&path, false);
                    }
                }
            }
            Action::Data => {
                config::private_dir(&config::home())?;
                sys::open(&config::home(), false);
            }
            Action::CheckUpdate => self.check_update(shell, true),
            Action::InstallUpdate => {
                let release = match &self.ui.borrow().update {
                    Update::Available(release) => Some(release.clone()),
                    _ => None,
                };
                if let Some(release) = release {
                    self.offer_update(shell, &release);
                }
            }
            Action::Tun | Action::Auto | Action::AutoUpdate => {
                let mut updated = self.ui.borrow().settings.clone();
                match action {
                    Action::Tun => updated.tun = !updated.tun,
                    Action::Auto => updated.auto_connect = !updated.auto_connect,
                    _ => updated.auto_update = !updated.auto_update,
                }
                updated.save()?;
                self.ui.borrow_mut().settings = updated;
            }
            Action::Mode(mode) => {
                let run = self.ui.borrow().run.clone();
                if let Some(run) = run {
                    let emit = self.emit.clone();
                    self.ui.borrow_mut().operation = true;
                    thread::spawn(move || {
                        match run
                            .api
                            .request("PATCH", "/configs", Some(json!({"mode":mode})))
                        {
                            Ok(_) => emit(Event::Mode(run.id.clone(), mode)),
                            Err(e) => emit(Event::Error(e.to_string())),
                        }
                    });
                } else {
                    let mut settings = self.ui.borrow().settings.clone();
                    settings.mode = mode;
                    settings.save()?;
                    self.ui.borrow_mut().settings = settings;
                }
            }
            Action::Select(group, choice) => {
                let run = self.ui.borrow().run.clone();
                if let Some(run) = run {
                    let emit = self.emit.clone();
                    self.ui.borrow_mut().operation = true;
                    thread::spawn(move || match run.api.select(&group, &choice) {
                        Ok(()) => emit(Event::Selection(
                            run.id.clone(),
                            group,
                            choice,
                            run.api.groups(&run.group_order()).unwrap_or_default(),
                        )),
                        Err(error) => emit(Event::Error(error.to_string())),
                    });
                }
            }
        }
        shell.render();
        Ok(())
    }
    pub fn connect(&self, shell: &dyn Shell) -> Result<()> {
        let phase_running = self.ui.borrow().phase == Phase::Running;
        if phase_running {
            let mut ui = self.ui.borrow_mut();
            if let Some(run) = &ui.run {
                run.cancel();
            }
            ui.phase = Phase::Stopping;
            ui.message = "正在停止…".into();
        } else {
            ensure!(self.stopped(), "连接正在处理中。");
            self.save_options(shell)?;
            let settings = self.ui.borrow().settings.clone();
            let run = engine::start(settings, self.emit.clone())?;
            let mut ui = self.ui.borrow_mut();
            ui.run = Some(run);
            ui.phase = Phase::Starting;
            ui.message = "正在检查配置…".into();
        }
        shell.render();
        Ok(())
    }
    /// Fetches the groups for a status menu that is about to open.
    pub fn refresh_groups(&self) {
        let run = {
            let mut ui = self.ui.borrow_mut();
            if ui.phase != Phase::Running || ui.refreshing {
                return;
            }
            ui.refreshing = true;
            ui.run.clone()
        };
        if let Some(run) = run {
            let emit = self.emit.clone();
            thread::spawn(move || {
                emit(Event::Groups(
                    run.id.clone(),
                    run.api.groups(&run.group_order()).unwrap_or_default(),
                ));
            });
        }
    }
    pub fn drain(&self, shell: &dyn Shell) {
        let events: Vec<_> = self.receiver.try_iter().collect();
        let mut changed = false;
        let mut groups_changed = false;
        let mut errors = vec![];
        let mut notices = vec![];
        let (mut offer, mut staged, mut due) = (None, None, false);
        for event in events {
            let current = self.ui.borrow().run.as_ref().map(|r| r.id.clone());
            let mut error = None;
            let mut handled = true;
            {
                let mut ui = self.ui.borrow_mut();
                match event {
                    Event::Progress(id, message) if current.as_deref() == Some(&id) => {
                        ui.message = message
                    }
                    Event::Ready(id, groups) if current.as_deref() == Some(&id) => {
                        ui.groups = groups;
                        ui.phase = Phase::Running;
                        ui.message = if ui.settings.tun {
                            "TUN 已连接"
                        } else {
                            "本地代理已启动"
                        }
                        .into();
                    }
                    Event::Stopped(id) if current.as_deref() == Some(&id) => {
                        ui.log = ui.run.as_ref().map(|r| r.log());
                        ui.run = None;
                        ui.groups.clear();
                        ui.refreshing = false;
                        ui.operation = false;
                        ui.phase = Phase::Stopped;
                        ui.message = "已停止".into();
                    }
                    Event::Failed(id, message) if current.as_deref() == Some(&id) => {
                        ui.log = ui.run.as_ref().map(|r| r.log());
                        ui.run = None;
                        ui.groups.clear();
                        ui.refreshing = false;
                        ui.operation = false;
                        ui.phase = Phase::Failed;
                        // Keep the tray menu and two-line footer short; recovery steps
                        // and paths belong in a dialog and the saved error log.
                        if let Some((summary, _)) = message.split_once('\n') {
                            ui.message = summary.into();
                            notices.push(message);
                        } else {
                            ui.message = message;
                        }
                    }
                    Event::Groups(id, groups) if current.as_deref() == Some(&id) => {
                        ui.refreshing = false;
                        // Only the menu shows groups; it may already be open.
                        handled = false;
                        if !groups.is_empty() && groups != ui.groups {
                            ui.groups = groups;
                            groups_changed = true;
                        }
                    }
                    Event::Mode(id, mode) if current.as_deref() == Some(&id) => {
                        ui.operation = false;
                        ui.settings.mode = mode;
                        error = ui.settings.save().err();
                    }
                    Event::Selection(id, group, choice, groups)
                        if current.as_deref() == Some(&id) =>
                    {
                        ui.operation = false;
                        ui.groups = groups;
                        ui.settings.selections.insert(group, choice);
                        error = ui.settings.save().err();
                    }
                    Event::Error(message) => {
                        ui.operation = false;
                        error = Some(anyhow::anyhow!(message));
                    }
                    Event::Update(update::Event::Due) => {
                        handled = false;
                        due = true;
                    }
                    Event::Update(update::Event::Checked(manual, result)) => {
                        ui.update = Update::Idle;
                        match result {
                            Ok(update::Check::Available(release)) => {
                                ui.checked = Some(SystemTime::now());
                                if manual || ui.announced.as_deref() != Some(release.version()) {
                                    ui.announced = Some(release.version().into());
                                    offer = Some(release.clone());
                                }
                                ui.update = Update::Available(release);
                            }
                            Ok(update::Check::Current) => {
                                ui.checked = Some(SystemTime::now());
                                if manual {
                                    notices.push(format!("已是最新版本（{}）。", update::CURRENT));
                                }
                            }
                            Ok(update::Check::Unavailable) => {
                                ui.update = Update::Unavailable;
                                if manual {
                                    notices.push(update::UNAVAILABLE.into());
                                }
                            }
                            Err(message) if manual => {
                                error = Some(anyhow::anyhow!("检查更新失败：{message}"))
                            }
                            // A failed automatic check retries on the next hourly tick.
                            Err(_) => {}
                        }
                    }
                    Event::Update(update::Event::Staged(result)) => {
                        if let Update::Downloading(release) =
                            std::mem::replace(&mut ui.update, Update::Idle)
                        {
                            ui.update = Update::Available(release);
                        }
                        match result {
                            Ok(staging) => staged = Some(staging),
                            Err(message) => error = Some(anyhow::anyhow!("更新失败：{message}")),
                        }
                    }
                    _ => handled = false,
                }
            }
            changed |= handled;
            errors.extend(error);
        }
        if changed {
            shell.render();
        }
        if groups_changed {
            shell.groups_changed();
        }
        // Alerts run a modal loop that can re-enter drain; showing them mid-batch would
        // apply this batch's older events after newer ones.
        for error in errors {
            self.error(shell, &error.to_string());
        }
        for notice in notices {
            shell.alert("FlowBar", &notice, &[]);
        }
        if let Some(release) = offer {
            self.offer_update(shell, &release);
        }
        if let Some(staging) = staged {
            self.install_update(shell, &staging);
        }
        if due {
            self.update_due(shell);
        }
        self.advance_smoke(shell);
    }
    fn advance_smoke(&self, shell: &dyn Shell) {
        let (step, phase_running, phase_stopped, failed, operation, mode, selection, failure) = {
            let ui = self.ui.borrow();
            (
                ui.smoke_step,
                ui.phase == Phase::Running,
                ui.phase == Phase::Stopped,
                ui.phase == Phase::Failed,
                ui.operation,
                ui.settings.mode.clone(),
                ui.settings.selections.get("策略 / 测试").cloned(),
                ui.message.clone(),
            )
        };
        let Some(step) = step else {
            return;
        };
        if failed {
            eprintln!("FAIL Rust UI smoke: {failure}");
            std::process::exit(1);
        }
        let action = match step {
            0 if phase_running => Some(Action::Mode("global".into())),
            1 if mode == "global" && !operation => Some(Action::Mode("rule".into())),
            2 if mode == "rule" && !operation => {
                Some(Action::Select("策略 / 测试".into(), "REJECT".into()))
            }
            3 if selection.as_deref() == Some("REJECT") && !operation => {
                Some(Action::Select("策略 / 测试".into(), "DIRECT".into()))
            }
            4 if selection.as_deref() == Some("DIRECT") && !operation => {
                shell.close_settings();
                assert!(!shell.settings_open());
                shell.show_settings();
                Some(Action::Connect)
            }
            5 if phase_stopped => {
                println!(
                    "PASS native Rust UI: settings toggles, mode changes, node selection, window close/reopen, core stop"
                );
                shell.quit();
                None
            }
            _ => None,
        };
        if let Some(action) = action {
            self.ui.borrow_mut().smoke_step = Some(step + 1);
            if let Err(error) = self.act(shell, action) {
                eprintln!("FAIL Rust UI smoke: {error:#}");
                std::process::exit(1);
            }
        }
    }
    pub fn save_options(&self, shell: &dyn Shell) -> Result<()> {
        let mut updated = self.ui.borrow().settings.clone();
        if let Some(form) = shell.form() {
            updated.auto_connect = form.auto_connect;
            updated.auto_update = form.auto_update;
            if self.stopped() {
                updated.tun = form.tun;
                let ports = form
                    .ports
                    .iter()
                    .map(|port| port.trim().parse::<u16>())
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .ok()
                    .filter(|ports| ports.len() == 3)
                    .context("端口须为 1024–65535 之间的整数。")?;
                updated.proxy_port = ports[0];
                updated.controller_port = ports[1];
                updated.dns_port = ports[2];
            }
        }
        updated.save()?;
        self.ui.borrow_mut().settings = updated;
        shell.render();
        Ok(())
    }
    /// Saves the settings window after a field loses focus. The failure alert takes focus
    /// from the field, so a failed save resets the fields instead of failing again.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn commit(&self, shell: &dyn Shell) {
        if self.committing.replace(true) {
            return;
        }
        if let Err(error) = self.save_options(shell) {
            self.error(shell, &error.to_string());
            self.reset_ports.set(true);
            shell.render();
        }
        self.committing.set(false);
    }
    fn import(&self, shell: &dyn Shell) -> Result<()> {
        ensure!(self.stopped(), "请先停止连接，再导入配置。");
        if let Some(path) = shell.choose_profile() {
            let settings = config::import_profile(&path, None)?;
            let mut ui = self.ui.borrow_mut();
            ui.settings = settings;
            ui.summary = profile_summary();
        }
        shell.render();
        Ok(())
    }
    /// Local proxy for update requests while it runs; TUN already routes them.
    fn update_proxy(&self) -> Option<u16> {
        let ui = self.ui.borrow();
        ui.run
            .as_ref()
            .filter(|run| ui.phase == Phase::Running && !run.settings.tun)
            .map(|run| run.settings.proxy_port)
    }
    fn update_due(&self, shell: &dyn Shell) {
        let due = {
            let ui = self.ui.borrow();
            ui.settings.auto_update
                && matches!(ui.update, Update::Idle)
                && ui.checked.is_none_or(|time| {
                    time.elapsed()
                        .map_or(true, |elapsed| elapsed >= UPDATE_INTERVAL)
                })
        };
        if due {
            self.check_update(shell, false);
        }
    }
    fn check_update(&self, shell: &dyn Shell, manual: bool) {
        let proxy = self.update_proxy();
        {
            let mut ui = self.ui.borrow_mut();
            if !matches!(ui.update, Update::Idle | Update::Unavailable) {
                return;
            }
            ui.update = Update::Checking;
        }
        shell.render();
        let emit = self.emit.clone();
        thread::spawn(move || {
            let result = update::check(proxy).map_err(|e| format!("{e:#}"));
            emit(Event::Update(update::Event::Checked(manual, result)));
        });
    }
    fn offer_update(&self, shell: &dyn Shell, release: &Release) {
        let body = release.body.as_deref().unwrap_or_default().trim();
        let mut notes: String = body.chars().take(600).collect();
        if notes.len() < body.len() {
            notes.push('…');
        }
        let choice = shell.alert(
            &format!("FlowBar {} 已发布", release.version()),
            format!(
                "当前版本 {}。安装时会断开连接，完成后自动重新打开 FlowBar。\n\n{notes}",
                update::CURRENT
            )
            .trim_end(),
            &["安装并重启", "稍后", "查看发布页"],
        );
        match choice {
            0 => self.download_update(shell),
            2 if release.html_url.starts_with("https://github.com/") => {
                sys::open_url(&release.html_url)
            }
            _ => {}
        }
    }
    fn download_update(&self, shell: &dyn Shell) {
        let proxy = self.update_proxy();
        let release = {
            let mut ui = self.ui.borrow_mut();
            let Update::Available(release) = &ui.update else {
                return;
            };
            let release = release.clone();
            ui.update = Update::Downloading(release.clone());
            release
        };
        shell.render();
        let emit = self.emit.clone();
        thread::spawn(move || {
            let result = update::stage(&release, proxy).map_err(|e| format!("{e:#}"));
            emit(Event::Update(update::Event::Staged(result)));
        });
    }
    fn install_update(&self, shell: &dyn Shell, staging: &Path) {
        match update::install(staging) {
            // Quitting stops the connection; the relaunch waits for this process to exit.
            Ok(()) => shell.quit(),
            Err(error) => self.error(shell, &format!("更新失败：{error:#}")),
        }
    }
    pub fn error(&self, shell: &dyn Shell, message: &str) {
        shell.alert("FlowBar", message, &[]);
    }

    /// The status menu, built just before it opens. Reading the login item touches the
    /// disk, so callers that also build a [`View`] pass its value.
    pub fn menu(&self, login: Login) -> Vec<Entry> {
        let ui = self.ui.borrow();
        let busy = matches!(ui.phase, Phase::Starting | Phase::Stopping);
        let enabled = !busy && !ui.operation;
        let stopped = matches!(ui.phase, Phase::Stopped | Phase::Failed);
        let mut entries = vec![
            item(&format!("FlowBar · {}", ui.message), None, false, None),
            item(&ui.settings.profile_name, None, false, None),
            Entry::Separator,
            item(
                if ui.phase == Phase::Running {
                    "停止连接"
                } else {
                    "启动连接"
                },
                Some(Action::Connect),
                enabled,
                None,
            ),
            Entry::Menu {
                title: "分流模式".into(),
                entries: [
                    ("规则分流", "rule"),
                    ("全局代理", "global"),
                    ("全部直连", "direct"),
                ]
                .into_iter()
                .map(|(title, mode)| {
                    item(
                        title,
                        Some(Action::Mode(mode.into())),
                        enabled,
                        Some(ui.settings.mode == mode),
                    )
                })
                .collect(),
                groups: false,
            },
        ];
        if !ui.groups.is_empty() {
            entries.push(Entry::Menu {
                title: "策略组与节点".into(),
                entries: group_entries(&ui.groups, enabled),
                groups: true,
            });
        }
        let (title, action) = update_entry(&ui.update);
        let available = action.is_some();
        entries.extend([
            Entry::Separator,
            item(
                "使用 TUN",
                Some(Action::Tun),
                stopped,
                Some(ui.settings.tun),
            ),
            item(
                "打开后自动连接",
                Some(Action::Auto),
                true,
                Some(ui.settings.auto_connect),
            ),
            item(
                if login == Login::RequiresApproval {
                    "登录时打开（等待系统批准）"
                } else {
                    "登录时打开 FlowBar"
                },
                Some(Action::Login),
                true,
                Some(login == Login::Enabled),
            ),
            Entry::Separator,
            item("设置…", Some(Action::Settings), true, None),
            item("导入 YAML…", Some(Action::Import), stopped, None),
            item(
                "打开运行日志",
                Some(Action::Logs),
                ui.run.is_some() || ui.log.is_some(),
                None,
            ),
            item("打开数据目录", Some(Action::Data), true, None),
            Entry::Separator,
            item(&title, action, available, None),
            item(
                "自动检查更新",
                Some(Action::AutoUpdate),
                true,
                Some(ui.settings.auto_update),
            ),
            Entry::Separator,
            item("退出 FlowBar", Some(Action::Quit), true, None),
        ]);
        entries
    }
    pub fn view(&self) -> View {
        let ui = self.ui.borrow();
        let busy = matches!(ui.phase, Phase::Starting | Phase::Stopping);
        let stopped = matches!(ui.phase, Phase::Stopped | Phase::Failed);
        let imported = ui.summary.is_some();
        let login = sys::login_status();
        let (update_title, update_action) = update_entry(&ui.update);
        View {
            imported,
            profile: if imported {
                ui.settings.profile_name.clone()
            } else {
                "尚未导入配置".into()
            },
            stats: ui
                .summary
                .clone()
                .unwrap_or_else(|| "导入完整的 mihomo YAML 配置后即可连接。".into()),
            editable: stopped,
            tun: ui.settings.tun,
            tun_hint: if ui.settings.tun {
                "接管所有应用的流量；连接时需要管理员授权。"
            } else {
                "只开放下方的本地 HTTP/SOCKS 端口，不修改系统代理，需在应用中手动指定。"
            },
            ports: [
                ui.settings.proxy_port,
                ui.settings.controller_port,
                ui.settings.dns_port,
            ],
            ports_hint: (!stopped).then_some("停止连接后可修改端口。"),
            login,
            auto_connect: ui.settings.auto_connect,
            login_hint: if login == Login::RequiresApproval {
                "需要在「系统设置 → 通用 → 登录项」中允许 FlowBar。"
            } else {
                sys::LOGIN_HINT
            },
            auto_update: ui.settings.auto_update,
            update_title,
            update_enabled: update_action.is_some(),
            phase: ui.phase,
            message: ui.message.clone(),
            logs: ui.run.is_some() || ui.log.is_some(),
            start_title: if busy {
                "处理中…"
            } else if ui.phase == Phase::Running {
                "停止连接"
            } else {
                "启动连接"
            },
            start_enabled: !busy,
        }
    }
    /// The update entry's action in the menu and settings window.
    pub fn update_action(&self) -> Option<Action> {
        update_entry(&self.ui.borrow().update).1
    }
}

/// Each group as a submenu of its choices; only selector groups accept a choice.
pub fn group_entries(groups: &[Group], enabled: bool) -> Vec<Entry> {
    groups
        .iter()
        .map(|group| Entry::Menu {
            title: group_title(group),
            entries: group
                .choices
                .iter()
                .map(|choice| {
                    item(
                        choice,
                        Some(Action::Select(group.name.clone(), choice.clone())),
                        enabled && selectable(group),
                        Some(&group.current == choice),
                    )
                })
                .collect(),
            groups: false,
        })
        .collect()
}
pub fn group_title(group: &Group) -> String {
    format!("{} → {}", group.name, group.current)
}
pub fn selectable(group: &Group) -> bool {
    group.kind.eq_ignore_ascii_case("selector")
}
/// Menu and settings window text for the update state, and the action it offers.
fn update_entry(update: &Update) -> (String, Option<Action>) {
    match update {
        Update::Idle => ("检查更新…".into(), Some(Action::CheckUpdate)),
        Update::Unavailable => ("更新源不可用，重试…".into(), Some(Action::CheckUpdate)),
        Update::Checking => ("正在检查更新…".into(), None),
        Update::Available(release) => (
            format!("安装更新 {}…", release.version()),
            Some(Action::InstallUpdate),
        ),
        Update::Downloading(release) => (format!("正在下载 {}…", release.version()), None),
    }
}
/// Counts shown in the settings window. Parsing a large profile is costly, so callers cache it.
fn profile_summary() -> Option<String> {
    let yaml = fs::read_to_string(config::home().join("profile.yaml")).ok()?;
    let summary = config::summary(&yaml).ok()?;
    Some(format!(
        "{} 个节点 · {} 个策略组 · {} 条规则",
        summary.proxies,
        summary.groups.len(),
        summary.rules
    ))
}

/// Registers and removes the login item; runs in the UI test's isolated environment.
#[cfg(target_os = "macos")]
fn login_roundtrip() -> Result<()> {
    macos::login_roundtrip()
}
#[cfg(not(target_os = "macos"))]
fn login_roundtrip() -> Result<()> {
    // Every FlowBar copy shares one entry, and the Windows registry is not isolated.
    ensure!(!sys::login_entry(), "已有 FlowBar 登录项，测试不会覆盖它");
    struct Remove;
    impl Drop for Remove {
        fn drop(&mut self) {
            if sys::login_entry() {
                let _ = sys::remove_login();
            }
        }
    }
    let _cleanup = Remove;
    sys::toggle_login()?;
    let registered = login_status();
    sys::toggle_login()?;
    ensure!(registered == Login::Enabled, "登录项注册失败");
    ensure!(login_status() == Login::Disabled, "登录项撤销失败");
    println!("PASS login item registration/unregistration");
    Ok(())
}
