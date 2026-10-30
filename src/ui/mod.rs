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
    collections::{HashMap, HashSet},
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
    Ipv6,
    Auto,
    Login,
    Logs,
    Data,
    CheckUpdate,
    InstallUpdate,
    AutoUpdate,
    Mode(String),
    Select(String, String),
    /// Makes another imported profile active.
    Profile(String),
    RemoveProfile,
    /// Tests the latency of a group's members.
    Delay(String),
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
        /// Text at the right edge, such as a node's latency.
        detail: Option<String>,
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
        detail: None,
        action,
        enabled,
        checked,
    }
}

/// What the settings window shows.
pub struct View {
    pub imported: bool,
    /// The settings window's profile list, the active profile's position in it, and whether
    /// another one can be chosen now. Without profiles it holds a disabled placeholder.
    pub profiles: Vec<String>,
    pub profile_index: Option<usize>,
    pub switchable: bool,
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
    /// The groups or their latencies changed while the status menu may be open; the status
    /// item's menu shows them, but the settings window does not.
    fn groups_changed(&self) {}
    /// Exercise the native latency control in the isolated UI smoke test.
    fn smoke_delay(&self, _group: &str) -> bool {
        false
    }
    /// Check the native widgets during the isolated cross-platform UI smoke test.
    fn verify_smoke_render(&self) {}
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
    /// Imported profile names, read at launch and after an import or removal.
    pub profiles: Vec<String>,
    /// Latencies from the last tests, in milliseconds; `None` for a node that failed.
    pub delays: HashMap<String, Option<u16>>,
    /// Groups whose latency test is running.
    testing: HashSet<String>,
    /// Connect again once the connection has stopped, after switching profiles.
    reconnect: bool,
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
impl Ui {
    /// Menu actions are available while no connection change or menu operation is pending.
    fn menu_enabled(&self) -> bool {
        !matches!(self.phase, Phase::Starting | Phase::Stopping) && !self.operation
    }
    /// Forgets the state of a connection that has ended.
    fn end_run(&mut self) {
        self.log = self.run.as_ref().map(|r| r.log());
        self.run = None;
        self.groups.clear();
        self.delays.clear();
        self.testing.clear();
        self.refreshing = false;
        self.operation = false;
    }
}
impl Default for Ui {
    fn default() -> Self {
        Self {
            settings: Settings::load(),
            phase: Phase::Stopped,
            message: "已停止".into(),
            groups: vec![],
            profiles: config::profiles(),
            delays: HashMap::new(),
            testing: HashSet::new(),
            reconnect: false,
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
            || !config::profile_path(&self.ui.borrow().settings.profile_name).exists()
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
    /// Reloads profiles and counts when the settings window opens or regains focus.
    pub fn load_summary(&self) {
        let mut ui = self.ui.borrow_mut();
        ui.profiles = config::profiles();
        ui.summary = profile_summary(&ui.settings.profile_name);
    }
    /// Whether the settings window should overwrite its port fields even while one is being
    /// edited, after a failed save. Only that window's render may take it.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn take_reset_ports(&self) -> bool {
        self.reset_ports.take()
    }

    /// Runs an action and shows its error, if any.
    pub fn perform(&self, shell: &dyn Shell, action: Action) {
        if let Some(run) = &self.ui.borrow().run {
            run.diagnostic(format!("UI action: {action:?}"));
        }
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
            Action::Reveal => {
                let path = config::profile_path(&self.ui.borrow().settings.profile_name);
                sys::open(&path, true)
            }
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
            Action::Tun | Action::Ipv6 | Action::Auto | Action::AutoUpdate => {
                let mut updated = self.ui.borrow().settings.clone();
                match action {
                    Action::Tun => updated.tun = !updated.tun,
                    Action::Ipv6 => updated.ipv6 = !updated.ipv6,
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
            Action::Profile(name) => self.switch_profile(shell, &name)?,
            Action::RemoveProfile => self.remove_profile(shell)?,
            Action::Delay(group) => {
                let run = {
                    let mut ui = self.ui.borrow_mut();
                    match ui.run.clone() {
                        Some(run)
                            if ui.phase == Phase::Running && ui.testing.insert(group.clone()) =>
                        {
                            Some(run)
                        }
                        _ => None,
                    }
                };
                if let Some(run) = run {
                    shell.groups_changed();
                    let emit = self.emit.clone();
                    thread::spawn(move || {
                        let started = std::time::Instant::now();
                        run.diagnostic(format!("latency test started: group={group:?}"));
                        let delays = run.api.group_delay(&group).map_err(|e| e.to_string());
                        run.diagnostic(format!(
                            "latency test finished in {} ms: group={group:?} result={delays:?}",
                            started.elapsed().as_millis()
                        ));
                        let groups = run.api.groups(&run.group_order()).unwrap_or_default();
                        emit(Event::Delays(run.id.clone(), group, delays, groups));
                    });
                }
            }
        }
        shell.render();
        Ok(())
    }
    /// Makes another imported profile active; a running connection restarts with it.
    fn switch_profile(&self, shell: &dyn Shell, name: &str) -> Result<()> {
        let running = {
            let ui = self.ui.borrow();
            ensure!(ui.menu_enabled(), "连接正在处理中。");
            if ui.settings.profile_name == name || !ui.profiles.iter().any(|p| p == name) {
                return Ok(());
            }
            ui.phase == Phase::Running
        };
        let mut settings = self.ui.borrow().settings.clone();
        settings.switch_profile(name);
        settings.save()?;
        {
            let mut ui = self.ui.borrow_mut();
            ui.summary = profile_summary(name);
            ui.settings = settings;
            ui.reconnect = running;
        }
        if running {
            self.connect(shell)?;
        }
        Ok(())
    }
    fn remove_profile(&self, shell: &dyn Shell) -> Result<()> {
        ensure!(self.stopped(), "请先停止连接，再删除配置。");
        let name = self.ui.borrow().settings.profile_name.clone();
        let choice = shell.alert(
            &format!("删除配置「{name}」？"),
            "只删除 FlowBar 保存的副本，原来的 YAML 文件不受影响。",
            &["删除", "取消"],
        );
        if choice != 0 {
            return Ok(());
        }
        let mut settings = self.ui.borrow().settings.clone();
        settings.remove_profile()?;
        let mut ui = self.ui.borrow_mut();
        ui.profiles = config::profiles();
        ui.summary = profile_summary(&settings.profile_name);
        ui.settings = settings;
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
        shell.groups_changed();
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
        let (mut offer, mut staged, mut due, mut restart) = (None, None, false, false);
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
                        groups_changed = true;
                        ui.phase = Phase::Running;
                        ui.message = if ui.settings.tun {
                            "TUN 已连接"
                        } else {
                            "本地代理已启动"
                        }
                        .into();
                    }
                    Event::Stopped(id) if current.as_deref() == Some(&id) => {
                        ui.end_run();
                        groups_changed = true;
                        ui.phase = Phase::Stopped;
                        ui.message = "已停止".into();
                        restart = std::mem::take(&mut ui.reconnect);
                    }
                    Event::Failed(id, message) if current.as_deref() == Some(&id) => {
                        ui.end_run();
                        groups_changed = true;
                        ui.reconnect = false;
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
                        groups_changed = true;
                        ui.settings.selections.insert(group, choice);
                        error = ui.settings.save().err();
                    }
                    Event::Delays(id, group, delays, groups) if current.as_deref() == Some(&id) => {
                        ui.testing.remove(&group);
                        if !groups.is_empty() {
                            ui.groups = groups;
                        }
                        match delays {
                            // A failed node gets a result too, replacing an older delay.
                            Ok(delays) => {
                                let Ui {
                                    groups,
                                    delays: shown,
                                    ..
                                } = &mut *ui;
                                for choice in groups
                                    .iter()
                                    .filter(|g| g.name == group)
                                    .flat_map(|g| &g.choices)
                                {
                                    shown.insert(choice.clone(), delays.get(choice).copied());
                                }
                            }
                            // A test fails when the connection stops during it.
                            Err(message) if ui.phase == Phase::Running => {
                                error = Some(anyhow::anyhow!("测试延迟失败：{message}"))
                            }
                            Err(_) => {}
                        }
                        // Only the menu shows latencies; it may already be open.
                        handled = false;
                        groups_changed = true;
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
        if restart {
            errors.extend(self.connect(shell).err());
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
        const GROUP: &str = "策略 / 测试";
        if self.ui.borrow().smoke_step.is_some() {
            shell.verify_smoke_render();
        }
        let (step, action) = {
            let ui = self.ui.borrow();
            let Some(step) = ui.smoke_step else {
                return;
            };
            if ui.phase == Phase::Failed {
                eprintln!("FAIL Rust UI smoke: {}", ui.message);
                std::process::exit(1);
            }
            let idle = !ui.operation;
            let running = ui.phase == Phase::Running && idle;
            let selection = ui.settings.selections.get(GROUP).map(String::as_str);
            // The test home holds two profiles.
            let other = || {
                ui.profiles
                    .iter()
                    .find(|name| **name != ui.settings.profile_name)
                    .map(|name| Action::Profile(name.clone()))
            };
            let action = match step {
                0 if running => Some(Action::Mode("global".into())),
                1 if ui.settings.mode == "global" && idle => Some(Action::Mode("rule".into())),
                2 if ui.settings.mode == "rule" && idle => {
                    Some(Action::Select(GROUP.into(), "REJECT".into()))
                }
                3 if selection == Some("REJECT") && idle => {
                    Some(Action::Select(GROUP.into(), "DIRECT".into()))
                }
                4 if selection == Some("DIRECT") && idle => Some(Action::Delay(GROUP.into())),
                // Each choice has a result, a delay or a failure, whether or not it passed.
                5 if ui.testing.is_empty() && ui.delays.len() == 2 => other(),
                // The other profile has no node choices of its own yet.
                6 if running && ui.settings.selections.is_empty() => other(),
                7 if running && selection == Some("DIRECT") => Some(Action::Connect),
                8 | 9 if ui.phase == Phase::Stopped => other(),
                10 if ui.phase == Phase::Stopped => Some(Action::Quit),
                _ => None,
            };
            (step, action)
        };
        let Some(action) = action else {
            return;
        };
        self.ui.borrow_mut().smoke_step = Some(step + 1);
        if action == Action::Quit {
            println!(
                "PASS native Rust UI: settings toggles, mode changes, node selection, latency test, profile switching, window close/reopen, core stop"
            );
            shell.quit();
            return;
        }
        if action == Action::Connect {
            shell.close_settings();
            assert!(!shell.settings_open());
            shell.show_settings();
        }
        if let Action::Delay(group) = &action {
            if shell.smoke_delay(group) {
                return;
            }
        }
        if let Err(error) = self.act(shell, action) {
            eprintln!("FAIL Rust UI smoke: {error:#}");
            std::process::exit(1);
        }
        // Switching a stopped profile is synchronous and emits no worker event.
        if self.stopped() {
            self.advance_smoke(shell);
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
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let mut name = config::profile_name(&stem);
            // Profile names are file names, which may ignore case.
            let profiles = self.ui.borrow().profiles.clone();
            let existing = |name: &str| {
                profiles
                    .iter()
                    .find(|p| p.to_lowercase() == name.to_lowercase())
                    .cloned()
            };
            let taken = |name: &str| existing(name).is_some();
            if let Some(current) = existing(&name) {
                let choice = shell.alert(
                    &format!("已有名为「{current}」的配置"),
                    "订阅更新后可以替换它；导入另一个同名文件时，可以另存为新配置。",
                    &["替换", "另存为新配置", "取消"],
                );
                match choice {
                    // Replace the existing copy under its own spelling, or a case-sensitive
                    // file system would keep both and others would show the old name.
                    0 => name = current,
                    1 => {
                        name = (2..)
                            .map(|number| format!("{name} {number}"))
                            .find(|name| !taken(name))
                            .unwrap()
                    }
                    _ => return Ok(()),
                }
            }
            let settings = config::import_profile(&path, Some(&name))?;
            let mut ui = self.ui.borrow_mut();
            ui.profiles = config::profiles();
            ui.summary = profile_summary(&settings.profile_name);
            ui.settings = settings;
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
        if let Some(run) = &self.ui.borrow().run {
            run.diagnostic(format!("UI error: {message}"));
        }
        shell.alert("FlowBar", message, &[]);
    }

    /// The status menu, built just before it opens. Reading the login item touches the
    /// disk, so callers that also build a [`View`] pass its value.
    pub fn menu(&self, login: Login) -> Vec<Entry> {
        let ui = self.ui.borrow();
        let enabled = ui.menu_enabled();
        let stopped = matches!(ui.phase, Phase::Stopped | Phase::Failed);
        let active = ui.profiles.contains(&ui.settings.profile_name);
        let mut profiles: Vec<_> = ui
            .profiles
            .iter()
            .map(|name| {
                item(
                    name,
                    Some(Action::Profile(name.clone())),
                    enabled,
                    Some(*name == ui.settings.profile_name),
                )
            })
            .collect();
        if !profiles.is_empty() {
            profiles.push(Entry::Separator);
        }
        profiles.push(item("导入 YAML…", Some(Action::Import), stopped, None));
        if active {
            profiles.push(item(
                "删除当前配置…",
                Some(Action::RemoveProfile),
                stopped,
                None,
            ));
        }
        let mut entries = vec![
            item(&format!("FlowBar · {}", ui.message), None, false, None),
            Entry::Menu {
                title: if active {
                    format!("配置：{}", ui.settings.profile_name)
                } else {
                    "配置".into()
                },
                entries: profiles,
                groups: false,
            },
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
                entries: group_entries(&ui),
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
                "使用 IPv6",
                Some(Action::Ipv6),
                stopped,
                Some(ui.settings.ipv6),
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
        let (profiles, profile_index) = if ui.profiles.is_empty() {
            (vec!["尚未导入配置".into()], Some(0))
        } else {
            let index = ui
                .profiles
                .iter()
                .position(|p| *p == ui.settings.profile_name);
            (ui.profiles.clone(), index)
        };
        View {
            imported,
            switchable: !ui.profiles.is_empty() && ui.menu_enabled(),
            profiles,
            profile_index,
            stats: ui
                .summary
                .clone()
                .unwrap_or_else(|| "导入完整的 mihomo YAML 配置后即可连接。".into()),
            editable: stopped,
            tun: ui.settings.tun,
            tun_hint: if ui.settings.tun {
                if cfg!(target_os = "macos") {
                    "接管所有应用的流量；首次安装或升级后台服务时授权，之后连接无需重复输入密码。"
                } else {
                    "接管所有应用的流量；连接时需要管理员授权。"
                }
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
    /// The 策略组与节点 submenu's entries, for a front end that refreshes it while open.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub fn group_entries(&self) -> Vec<Entry> {
        group_entries(&self.ui.borrow())
    }
}

/// Each group as a submenu: a latency test, then its choices with their latencies once
/// tested. Only selector groups accept a choice.
fn group_entries(ui: &Ui) -> Vec<Entry> {
    let enabled = ui.menu_enabled();
    ui.groups
        .iter()
        .map(|group| {
            let testing = ui.testing.contains(&group.name);
            let mut entries = vec![
                item(
                    if testing {
                        "正在测试延迟…"
                    } else {
                        "测试延迟"
                    },
                    Some(Action::Delay(group.name.clone())),
                    enabled && !testing,
                    None,
                ),
                Entry::Separator,
            ];
            entries.extend(group.choices.iter().map(|choice| Entry::Item {
                title: choice.clone(),
                detail: ui.delays.get(choice).map(|delay| delay_text(*delay)),
                action: Some(Action::Select(group.name.clone(), choice.clone())),
                enabled: enabled && group.kind.eq_ignore_ascii_case("selector"),
                checked: Some(&group.current == choice),
            }));
            Entry::Menu {
                title: format!("{} → {}", group.name, group.current),
                entries,
                groups: false,
            }
        })
        .collect()
}
fn delay_text(delay: Option<u16>) -> String {
    delay.map_or_else(|| "超时".into(), |ms| format!("{ms} ms"))
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
fn profile_summary(name: &str) -> Option<String> {
    let yaml = fs::read_to_string(config::profile_path(name)).ok()?;
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
