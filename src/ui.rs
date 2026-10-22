use crate::{
    api::Group,
    config::{self, Settings},
    engine::{self, Emit, Event, Run},
};
use anyhow::{Result, ensure};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    sel,
};
use objc2_app_kit::*;
use objc2_foundation::{
    NSArray, NSDictionary, NSDistributedNotificationCenter, NSNotification, NSObject,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicPtr, Ordering},
        mpsc,
    },
    thread,
};

static DELEGATE: AtomicPtr<Delegate> = AtomicPtr::new(std::ptr::null_mut());
#[derive(PartialEq, Eq)]
enum Phase {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}
#[derive(Serialize, Deserialize)]
enum Action {
    Connect,
    Settings,
    Import,
    Quit,
    Tun,
    Auto,
    Login,
    Logs,
    Data,
    Mode(String),
    Select(String, String),
}
struct Window {
    native: Retained<NSWindow>,
    profile: Retained<NSTextField>,
    status: Retained<NSTextField>,
    tun: Retained<NSButton>,
    auto: Retained<NSButton>,
    login: Retained<NSButton>,
    login_hint: Retained<NSTextField>,
    ports: Vec<Retained<NSTextField>>,
    start: Retained<NSButton>,
    import: Retained<NSButton>,
}
struct Ui {
    settings: Settings,
    phase: Phase,
    message: String,
    groups: Vec<Group>,
    run: Option<Arc<Run>>,
    log: Option<PathBuf>,
    status_item: Option<Retained<NSStatusItem>>,
    menu: Option<Retained<NSMenu>>,
    window: Option<Window>,
    refreshing: bool,
    operation: bool,
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
            status_item: None,
            menu: None,
            window: None,
            refreshing: false,
            operation: false,
            smoke_step: std::env::args().any(|a| a == "--ui-smoke").then_some(0),
        }
    }
}
struct Ivars {
    ui: RefCell<Ui>,
    receiver: mpsc::Receiver<Event>,
    emit: Emit,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}
    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_launch(&self, _notification: &NSNotification) {
            self.setup();
        }
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _application: &NSApplication) -> NSApplicationTerminateReply {
            if let Some(run) = &self.ivars().ui.borrow().run { run.cancel(); }
            NSApplicationTerminateReply::TerminateNow
        }
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn reopen(&self, _application: &NSApplication, _visible: bool) -> bool { self.show_settings(); true }
    }
    unsafe impl NSMenuDelegate for Delegate {
        #[unsafe(method(menuWillOpen:))]
        fn will_open(&self, _menu: &NSMenu) { self.refresh_groups(); }
    }
    unsafe impl NSWindowDelegate for Delegate {
        #[unsafe(method(windowWillClose:))]
        fn close_settings(&self, _notification: &NSNotification) { self.ivars().ui.borrow_mut().window = None; }
    }
    impl Delegate {
        #[unsafe(method(menuAction:))]
        fn menu_action(&self, item: &NSMenuItem) {
            let action = item.representedObject().and_then(|s| s.downcast::<NSString>().ok())
                .and_then(|s| serde_json::from_str::<Action>(&s.to_string()).ok());
            if let Some(action) = action { if let Err(error) = self.act(action) { self.error(&error.to_string()); } }
        }
        #[unsafe(method(toggleConnection:))]
        fn connection_button(&self, _sender: &AnyObject) {
            if let Err(error) = self.connect() { self.error(&error.to_string()); }
        }
        #[unsafe(method(importYaml:))]
        fn import_button(&self, _sender: &AnyObject) { if let Err(error) = self.import() { self.error(&error.to_string()); } }
        #[unsafe(method(optionsChanged:))]
        fn options_button(&self, _sender: &AnyObject) { if let Err(error) = self.save_options() { self.error(&error.to_string()); self.render(); } }
        #[unsafe(method(loginChanged:))]
        fn login_button(&self, _sender: &AnyObject) { if let Err(error) = self.toggle_login() { self.error(&error.to_string()); } self.render(); }
        #[unsafe(method(revealProfile:))]
        fn reveal_button(&self, _sender: &AnyObject) { open(&config::home().join("profile.yaml"), true); }
        #[unsafe(method(showSettingsNotification:))]
        fn settings_notification(&self, _notification: &NSNotification) { self.show_settings(); }
    }
);

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    struct Background;
    unsafe impl NSObjectProtocol for Background {}
    impl Background {
        #[unsafe(method(isOpaque))]
        fn opaque(&self) -> bool { true }
        #[unsafe(method(drawRect:))]
        fn draw_background(&self, rect: NSRect) {
            NSColor::windowBackgroundColor().setFill();
            NSRectFill(rect);
        }
    }
);

impl Delegate {
    fn setup(&self) {
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(24.0);
        self.ivars().ui.borrow_mut().status_item = Some(item);
        unsafe {
            NSDistributedNotificationCenter::defaultCenter().addObserver_selector_name_object(
                self,
                sel!(showSettingsNotification:),
                Some(&NSString::from_str("dev.local.FlowBar.showSettings")),
                None,
            );
        }
        self.render();
        let arguments: Vec<_> = std::env::args().collect();
        if arguments.iter().any(|a| a == "--ui-smoke") {
            self.show_settings();
            let check = (|| -> Result<()> {
                if std::env::var_os("FLOWBAR_TEST_LOGIN").is_some() {
                    login_roundtrip()?;
                }
                self.act(Action::Auto)?;
                self.act(Action::Auto)?;
                self.act(Action::Tun)?;
                self.act(Action::Tun)?;
                ensure!(
                    !self.ivars().ui.borrow().settings.tun,
                    "UI 测试必须保持 TUN 关闭"
                );
                self.connect()
            })();
            if let Err(error) = check {
                eprintln!("FAIL Rust UI smoke: {error:#}");
                std::process::exit(1);
            }
            return;
        }
        if arguments
            .iter()
            .any(|a| a == "--show-settings" || a == "--snapshot")
            || !config::home().join("profile.yaml").exists()
        {
            self.show_settings();
        }
        if let Some(index) = arguments.iter().position(|a| a == "--snapshot") {
            if let Some(path) = arguments.get(index + 1) {
                self.snapshot(Path::new(path));
            }
        }
        let auto_connect = self.ivars().ui.borrow().settings.auto_connect;
        if auto_connect && !arguments.iter().any(|a| a == "--no-autoconnect") {
            if let Err(error) = self.connect() {
                self.error(&error.to_string());
            }
        }
    }
    fn render(&self) {
        let ui = self.ivars().ui.borrow();
        let mtm = self.mtm();
        let busy = matches!(ui.phase, Phase::Starting | Phase::Stopping);
        let enabled = !busy && !ui.operation;
        if let Some(button) = ui.status_item.as_ref().and_then(|item| item.button(mtm)) {
            let symbol = if ui.phase == Phase::Running {
                "paperplane.fill"
            } else {
                "paperplane"
            };
            if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str(symbol),
                Some(&NSString::from_str("FlowBar")),
            ) {
                image.setTemplate(true);
                image.setSize(NSSize::new(18.0, 18.0));
                button.setImage(Some(&image));
            } else {
                button.setTitle(&NSString::from_str("↗"));
            }
            button.setToolTip(Some(&NSString::from_str(&format!(
                "FlowBar · {}",
                ui.message
            ))));
        }
        let created = ui.menu.is_none();
        let menu = ui.menu.clone().unwrap_or_else(|| {
            let menu = NSMenu::new(mtm);
            menu.setAutoenablesItems(false);
            menu.setDelegate(Some(ProtocolObject::from_ref(self)));
            menu
        });
        // Rebuild in place: replacing the status item's menu while it is open leaves the
        // displayed copy stale (groups refreshed in menuWillOpen never appeared).
        menu.removeAllItems();
        menu.addItem(&self.item(&format!("FlowBar · {}", ui.message), None, false, false));
        menu.addItem(&self.item(&ui.settings.profile_name, None, false, false));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&self.item(
            if ui.phase == Phase::Running {
                "停止连接"
            } else {
                "启动连接"
            },
            Some(Action::Connect),
            enabled,
            false,
        ));
        let modes = NSMenu::new(mtm);
        modes.setAutoenablesItems(false);
        for (title, mode) in [
            ("规则分流", "rule"),
            ("全局代理", "global"),
            ("全部直连", "direct"),
        ] {
            modes.addItem(&self.item(
                title,
                Some(Action::Mode(mode.into())),
                enabled,
                ui.settings.mode == mode,
            ));
        }
        let parent = self.item("分流模式", None, true, false);
        parent.setSubmenu(Some(&modes));
        menu.addItem(&parent);
        if !ui.groups.is_empty() {
            let groups = NSMenu::new(mtm);
            groups.setAutoenablesItems(false);
            for group in &ui.groups {
                let choices = NSMenu::new(mtm);
                choices.setAutoenablesItems(false);
                for choice in &group.choices {
                    choices.addItem(&self.item(
                        choice,
                        Some(Action::Select(group.name.clone(), choice.clone())),
                        enabled && group.kind.eq_ignore_ascii_case("selector"),
                        &group.current == choice,
                    ));
                }
                let item = self.item(
                    &format!("{} → {}", group.name, group.current),
                    None,
                    true,
                    false,
                );
                item.setSubmenu(Some(&choices));
                groups.addItem(&item);
            }
            let parent = self.item("策略组与节点", None, true, false);
            parent.setSubmenu(Some(&groups));
            menu.addItem(&parent);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let stopped = matches!(ui.phase, Phase::Stopped | Phase::Failed);
        menu.addItem(&self.item("使用 TUN", Some(Action::Tun), stopped, ui.settings.tun));
        menu.addItem(&self.item(
            "打开后自动连接",
            Some(Action::Auto),
            true,
            ui.settings.auto_connect,
        ));
        let login = login_status();
        let login_title = if login == SMAppServiceStatus::RequiresApproval {
            "登录时打开（等待系统批准）"
        } else {
            "登录时打开 FlowBar"
        };
        menu.addItem(&self.item(
            login_title,
            Some(Action::Login),
            true,
            login == SMAppServiceStatus::Enabled,
        ));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        for (title, action, enabled) in [
            ("设置…", Action::Settings, true),
            ("导入 YAML…", Action::Import, stopped),
            (
                "打开运行日志",
                Action::Logs,
                ui.run.is_some() || ui.log.is_some(),
            ),
            ("打开数据目录", Action::Data, true),
        ] {
            menu.addItem(&self.item(title, Some(action), enabled, false));
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let quit = self.item("退出 FlowBar", Some(Action::Quit), true, false);
        quit.setKeyEquivalent(&NSString::from_str("q"));
        menu.addItem(&quit);
        if created {
            if let Some(item) = &ui.status_item {
                item.setMenu(Some(&menu));
            }
        }
        if let Some(window) = &ui.window {
            let metadata = fs::read_to_string(config::home().join("profile.yaml"))
                .ok()
                .and_then(|s| config::summary(&s).ok());
            let description = metadata
                .map(|s| {
                    format!(
                        "{}\n{} 个节点 · {} 个策略组 · {} 条规则",
                        ui.settings.profile_name,
                        s.proxies,
                        s.groups.len(),
                        s.rules
                    )
                })
                .unwrap_or_else(|| "请导入完整的 mihomo YAML 配置".into());
            window
                .profile
                .setStringValue(&NSString::from_str(&description));
            window
                .status
                .setStringValue(&NSString::from_str(&ui.message));
            let color = if ui.phase == Phase::Failed {
                NSColor::systemRedColor()
            } else {
                NSColor::secondaryLabelColor()
            };
            window.status.setTextColor(Some(&color));
            window.tun.setState(i64::from(ui.settings.tun) as _);
            window.tun.setEnabled(stopped);
            window
                .auto
                .setState(i64::from(ui.settings.auto_connect) as _);
            window
                .login
                .setState(i64::from(login == SMAppServiceStatus::Enabled) as _);
            window.login_hint.setStringValue(&NSString::from_str(
                if login == SMAppServiceStatus::RequiresApproval {
                    "请在系统设置 → 通用 → 登录项中允许 FlowBar。"
                } else {
                    "登录自启与自动连接分别设置；TUN 连接仍需要管理员授权。"
                },
            ));
            for (field, port) in window.ports.iter().zip([
                ui.settings.proxy_port,
                ui.settings.controller_port,
                ui.settings.dns_port,
            ]) {
                if field.currentEditor().is_none() {
                    field.setStringValue(&NSString::from_str(&port.to_string()));
                }
                field.setEnabled(stopped);
            }
            window.start.setTitle(&NSString::from_str(if busy {
                "处理中…"
            } else if ui.phase == Phase::Running {
                "停止连接"
            } else {
                "启动连接"
            }));
            window.start.setEnabled(!busy);
            window.import.setEnabled(stopped);
        }
        drop(ui);
        self.ivars().ui.borrow_mut().menu = Some(menu);
    }
    fn item(
        &self,
        title: &str,
        action: Option<Action>,
        enabled: bool,
        checked: bool,
    ) -> Retained<NSMenuItem> {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(self.mtm()),
                &NSString::from_str(title),
                action.as_ref().map(|_| sel!(menuAction:)),
                &NSString::from_str(""),
            )
        };
        if let Some(action) = action {
            unsafe {
                item.setTarget(Some(self));
                item.setRepresentedObject(Some(&NSString::from_str(
                    &serde_json::to_string(&action).unwrap(),
                )));
            }
        }
        item.setEnabled(enabled);
        item.setState(i64::from(checked) as _);
        item
    }
    fn act(&self, action: Action) -> Result<()> {
        match action {
            Action::Connect => self.connect()?,
            Action::Settings => self.show_settings(),
            Action::Import => self.import()?,
            Action::Quit => NSApplication::sharedApplication(self.mtm()).terminate(None),
            Action::Login => self.toggle_login()?,
            Action::Logs => {
                let ui = self.ivars().ui.borrow();
                let path = ui.run.as_ref().map(|r| r.log()).or_else(|| ui.log.clone());
                if let Some(path) = path {
                    if path.exists() {
                        open(&path, false);
                    }
                }
            }
            Action::Data => {
                config::private_dir(&config::home())?;
                open(&config::home(), false);
            }
            Action::Tun | Action::Auto => {
                let mut updated = self.ivars().ui.borrow().settings.clone();
                if matches!(action, Action::Tun) {
                    updated.tun = !updated.tun;
                } else {
                    updated.auto_connect = !updated.auto_connect;
                }
                updated.save()?;
                self.ivars().ui.borrow_mut().settings = updated;
            }
            Action::Mode(mode) => {
                let run = self.ivars().ui.borrow().run.clone();
                if let Some(run) = run {
                    let emit = self.ivars().emit.clone();
                    self.ivars().ui.borrow_mut().operation = true;
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
                    let mut settings = self.ivars().ui.borrow().settings.clone();
                    settings.mode = mode;
                    settings.save()?;
                    self.ivars().ui.borrow_mut().settings = settings;
                }
            }
            Action::Select(group, choice) => {
                let run = self.ivars().ui.borrow().run.clone();
                if let Some(run) = run {
                    let emit = self.ivars().emit.clone();
                    self.ivars().ui.borrow_mut().operation = true;
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
        self.render();
        Ok(())
    }
    fn connect(&self) -> Result<()> {
        let phase_running = self.ivars().ui.borrow().phase == Phase::Running;
        if phase_running {
            let mut ui = self.ivars().ui.borrow_mut();
            if let Some(run) = &ui.run {
                run.cancel();
            }
            ui.phase = Phase::Stopping;
            ui.message = "正在停止…".into();
        } else {
            ensure!(
                matches!(
                    self.ivars().ui.borrow().phase,
                    Phase::Stopped | Phase::Failed
                ),
                "连接正在处理中。"
            );
            self.save_options()?;
            let settings = self.ivars().ui.borrow().settings.clone();
            let run = engine::start(settings, self.ivars().emit.clone())?;
            let mut ui = self.ivars().ui.borrow_mut();
            ui.run = Some(run);
            ui.phase = Phase::Starting;
            ui.message = "正在检查配置…".into();
        }
        self.render();
        Ok(())
    }
    fn refresh_groups(&self) {
        let run = {
            let mut ui = self.ivars().ui.borrow_mut();
            if ui.phase != Phase::Running || ui.refreshing {
                return;
            }
            ui.refreshing = true;
            ui.run.clone()
        };
        if let Some(run) = run {
            let emit = self.ivars().emit.clone();
            thread::spawn(move || {
                emit(Event::Groups(
                    run.id.clone(),
                    run.api.groups(&run.group_order()).unwrap_or_default(),
                ));
            });
        }
    }
    fn drain(&self) {
        let events: Vec<_> = self.ivars().receiver.try_iter().collect();
        let mut changed = false;
        let mut errors = vec![];
        for event in events {
            let current = self.ivars().ui.borrow().run.as_ref().map(|r| r.id.clone());
            let mut error = None;
            let mut handled = true;
            {
                let mut ui = self.ivars().ui.borrow_mut();
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
                        ui.message = message;
                    }
                    Event::Groups(id, groups) if current.as_deref() == Some(&id) => {
                        ui.refreshing = false;
                        // Rebuilding an open menu closes its submenus; skip it when nothing changed.
                        handled = !groups.is_empty() && groups != ui.groups;
                        if handled {
                            ui.groups = groups;
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
                    _ => handled = false,
                }
            }
            changed |= handled;
            errors.extend(error);
        }
        if changed {
            self.render();
        }
        // Alerts run a modal loop that can re-enter drain; showing them mid-batch would
        // apply this batch's older events after newer ones.
        for error in errors {
            self.error(&error.to_string());
        }
        self.advance_smoke();
    }
    fn advance_smoke(&self) {
        let (step, phase_running, phase_stopped, failed, operation, mode, selection, failure) = {
            let ui = self.ivars().ui.borrow();
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
                let window = self
                    .ivars()
                    .ui
                    .borrow()
                    .window
                    .as_ref()
                    .map(|w| w.native.clone());
                if let Some(window) = window {
                    window.close();
                }
                assert!(self.ivars().ui.borrow().window.is_none());
                self.show_settings();
                Some(Action::Connect)
            }
            5 if phase_stopped => {
                println!(
                    "PASS native Rust UI: settings toggles, mode changes, node selection, window close/reopen, core stop"
                );
                NSApplication::sharedApplication(self.mtm()).terminate(None);
                None
            }
            _ => None,
        };
        if let Some(action) = action {
            self.ivars().ui.borrow_mut().smoke_step = Some(step + 1);
            if let Err(error) = self.act(action) {
                eprintln!("FAIL Rust UI smoke: {error:#}");
                std::process::exit(1);
            }
        }
    }
    fn save_options(&self) -> Result<()> {
        let mut updated = self.ivars().ui.borrow().settings.clone();
        {
            let ui = self.ivars().ui.borrow();
            if let Some(window) = &ui.window {
                updated.auto_connect = window.auto.state() == 1;
                if matches!(ui.phase, Phase::Stopped | Phase::Failed) {
                    updated.tun = window.tun.state() == 1;
                    let ports = window
                        .ports
                        .iter()
                        .map(|f| f.stringValue().to_string().parse::<u16>())
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    updated.proxy_port = ports[0];
                    updated.controller_port = ports[1];
                    updated.dns_port = ports[2];
                }
            }
        }
        updated.save()?;
        self.ivars().ui.borrow_mut().settings = updated;
        self.render();
        Ok(())
    }
    #[allow(deprecated)]
    fn import(&self) -> Result<()> {
        ensure!(
            matches!(
                self.ivars().ui.borrow().phase,
                Phase::Stopped | Phase::Failed
            ),
            "请先停止连接，再导入配置。"
        );
        let panel = NSOpenPanel::openPanel(self.mtm());
        panel.setAllowsMultipleSelection(false);
        panel.setCanChooseDirectories(false);
        panel.setAllowedFileTypes(Some(&NSArray::from_retained_slice(&[
            NSString::from_str("yaml"),
            NSString::from_str("yml"),
        ])));
        if panel.runModal() == 1 {
            if let Some(path) = panel.URL().and_then(|url| url.path()) {
                let settings = config::import_profile(Path::new(&path.to_string()), None)?;
                self.ivars().ui.borrow_mut().settings = settings;
            }
        }
        self.render();
        Ok(())
    }
    fn toggle_login(&self) -> Result<()> {
        let service = unsafe { SMAppService::mainAppService() };
        unsafe {
            match service.status() {
                SMAppServiceStatus::Enabled => service
                    .unregisterAndReturnError()
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?,
                SMAppServiceStatus::RequiresApproval => {
                    SMAppService::openSystemSettingsLoginItems()
                }
                _ => {
                    service
                        .registerAndReturnError()
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                    if service.status() == SMAppServiceStatus::RequiresApproval {
                        SMAppService::openSystemSettingsLoginItems();
                    }
                }
            }
        }
        self.render();
        Ok(())
    }
    fn show_settings(&self) {
        if self.ivars().ui.borrow().window.is_none() {
            self.make_window();
        }
        self.render();
        if let Some(window) = &self.ivars().ui.borrow().window {
            window.native.center();
            window.native.makeKeyAndOrderFront(None);
        }
        #[allow(deprecated)]
        NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
    }
    fn make_window(&self) {
        let mtm = self.mtm();
        let native = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, 560.0, 570.0),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        native.setTitle(&NSString::from_str("FlowBar"));
        unsafe {
            native.setReleasedWhenClosed(false);
        }
        native.setDelegate(Some(ProtocolObject::from_ref(self)));
        let view: Retained<Background> = unsafe {
            msg_send![Background::alloc(mtm), initWithFrame: rect(0.0, 0.0, 560.0, 570.0)]
        };
        native.setContentView(Some(&view));
        let title = self.label("FlowBar", 28.0, false, rect(28.0, 500.0, 500.0, 40.0));
        title.setFont(Some(&NSFont::boldSystemFontOfSize(28.0)));
        view.addSubview(&title);
        view.addSubview(&self.label(
            "Rust 原生菜单栏 · mihomo 负责分流",
            13.0,
            true,
            rect(28.0, 470.0, 500.0, 24.0),
        ));
        let profile = self.label("", 13.0, false, rect(28.0, 410.0, 500.0, 44.0));
        view.addSubview(&profile);
        let import = self.button(
            "导入 YAML…",
            sel!(importYaml:),
            rect(22.0, 363.0, 126.0, 34.0),
            false,
        );
        view.addSubview(&import);
        view.addSubview(&self.button(
            "查看配置文件",
            sel!(revealProfile:),
            rect(150.0, 363.0, 140.0, 34.0),
            false,
        ));
        let tun = self.button(
            "使用 TUN 接管流量",
            sel!(optionsChanged:),
            rect(28.0, 323.0, 480.0, 25.0),
            true,
        );
        view.addSubview(&tun);
        view.addSubview(&self.label(
            "开启 TUN 需要管理员授权。关闭后仅提供本地 HTTP/SOCKS 代理端口。",
            12.0,
            true,
            rect(28.0, 278.0, 500.0, 35.0),
        ));
        let mut ports = vec![];
        for (index, title) in ["代理", "控制", "DNS"].iter().enumerate() {
            let x = 28.0 + index as f64 * 160.0;
            view.addSubview(&self.label(title, 12.0, false, rect(x, 237.0, 36.0, 24.0)));
            let field = NSTextField::initWithFrame(
                NSTextField::alloc(mtm),
                rect(x + 39.0, 237.0, 82.0, 24.0),
            );
            unsafe {
                field.setTarget(Some(self));
                field.setAction(Some(sel!(optionsChanged:)));
            }
            view.addSubview(&field);
            ports.push(field);
        }
        let auto = self.button(
            "打开 FlowBar 后自动连接",
            sel!(optionsChanged:),
            rect(28.0, 194.0, 500.0, 25.0),
            true,
        );
        view.addSubview(&auto);
        let login = self.button(
            "登录时打开 FlowBar",
            sel!(loginChanged:),
            rect(28.0, 154.0, 500.0, 25.0),
            true,
        );
        view.addSubview(&login);
        let login_hint = self.label("", 11.0, true, rect(28.0, 118.0, 500.0, 28.0));
        view.addSubview(&login_hint);
        let status = self.label("", 12.0, true, rect(28.0, 72.0, 500.0, 36.0));
        view.addSubview(&status);
        let start = self.button(
            "启动连接",
            sel!(toggleConnection:),
            rect(22.0, 25.0, 140.0, 36.0),
            false,
        );
        view.addSubview(&start);
        self.ivars().ui.borrow_mut().window = Some(Window {
            native,
            profile,
            status,
            tun,
            auto,
            login,
            login_hint,
            ports,
            start,
            import,
        });
    }
    fn label(
        &self,
        title: &str,
        size: f64,
        secondary: bool,
        frame: NSRect,
    ) -> Retained<NSTextField> {
        let field = NSTextField::wrappingLabelWithString(&NSString::from_str(title), self.mtm());
        field.setFont(Some(&NSFont::systemFontOfSize(size)));
        field.setFrame(frame);
        if secondary {
            field.setTextColor(Some(&NSColor::secondaryLabelColor()));
        }
        field
    }
    fn button(
        &self,
        title: &str,
        action: objc2::runtime::Sel,
        frame: NSRect,
        checkbox: bool,
    ) -> Retained<NSButton> {
        let button = unsafe {
            if checkbox {
                NSButton::checkboxWithTitle_target_action(
                    &NSString::from_str(title),
                    Some(self),
                    Some(action),
                    self.mtm(),
                )
            } else {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(title),
                    Some(self),
                    Some(action),
                    self.mtm(),
                )
            }
        };
        button.setFrame(frame);
        button
    }
    fn error(&self, message: &str) {
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&NSString::from_str("FlowBar"));
        alert.setInformativeText(&NSString::from_str(message));
        alert.runModal();
    }
    fn snapshot(&self, path: &Path) {
        let ui = self.ivars().ui.borrow();
        if let Some(window) = &ui.window {
            window.native.displayIfNeeded();
            if let Some(view) = window.native.contentView() {
                if let Some(bitmap) = view.bitmapImageRepForCachingDisplayInRect(view.bounds()) {
                    view.cacheDisplayInRect_toBitmapImageRep(view.bounds(), &bitmap);
                    if let Some(data) = unsafe {
                        bitmap.representationUsingType_properties(
                            NSBitmapImageFileType::PNG,
                            &NSDictionary::new(),
                        )
                    } {
                        let _ = config::private_write(path, &data.to_vec());
                    }
                }
            }
        }
    }
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}
fn open(path: &Path, reveal: bool) {
    let mut command = Command::new("/usr/bin/open");
    if reveal {
        command.arg("-R");
    }
    let _ = command.arg(path).spawn();
}
pub fn login_status() -> SMAppServiceStatus {
    unsafe { SMAppService::mainAppService().status() }
}

fn login_roundtrip() -> Result<()> {
    let identifier = objc2_foundation::NSBundle::mainBundle()
        .bundleIdentifier()
        .map(|s| s.to_string())
        .unwrap_or_default();
    ensure!(
        identifier.starts_with("dev.local.FlowBar.Validation."),
        "登录项验证只能在独立测试应用中执行"
    );
    let service = unsafe { SMAppService::mainAppService() };
    ensure!(
        unsafe { service.status() } != SMAppServiceStatus::Enabled,
        "测试登录项已存在"
    );
    struct Unregister(Retained<SMAppService>);
    impl Drop for Unregister {
        fn drop(&mut self) {
            let _ = unsafe { self.0.unregisterAndReturnError() };
        }
    }
    let cleanup = Unregister(service.clone());
    unsafe { service.registerAndReturnError() }
        .map_err(|e| anyhow::anyhow!("登录项注册失败：{e}"))?;
    let status = unsafe { service.status() };
    ensure!(
        status == SMAppServiceStatus::Enabled || status == SMAppServiceStatus::RequiresApproval,
        "登录项状态无效：{status:?}"
    );
    unsafe { service.unregisterAndReturnError() }
        .map_err(|e| anyhow::anyhow!("登录项撤销失败：{e}"))?;
    drop(cleanup);
    println!("PASS native login item registration/unregistration in isolated test app");
    Ok(())
}

pub fn run() -> Result<()> {
    let mtm = MainThreadMarker::new().expect("AppKit must run on the main thread");
    let name = NSString::from_str("dev.local.FlowBar");
    let applications = NSRunningApplication::runningApplicationsWithBundleIdentifier(&name);
    if !std::env::args().any(|a| a == "--ui-smoke")
        && (0..applications.count()).any(|index| {
            applications.objectAtIndex(index).processIdentifier() != std::process::id() as i32
        })
    {
        unsafe {
            NSDistributedNotificationCenter::defaultCenter()
                .postNotificationName_object_userInfo_deliverImmediately(
                    &NSString::from_str("dev.local.FlowBar.showSettings"),
                    None,
                    None,
                    true,
                );
        }
        return Ok(());
    }
    let (sender, receiver) = mpsc::channel();
    let emit: Emit = Arc::new(move |event| {
        if sender.send(event).is_err() {
            return;
        }
        dispatch2::DispatchQueue::main().exec_async(|| {
            let pointer = DELEGATE.load(Ordering::SeqCst);
            // Only dereferenced on the main queue; the delegate is retained for the entire AppKit run loop.
            if !pointer.is_null() {
                unsafe { &*pointer }.drain();
            }
        });
    });
    let this = Delegate::alloc(mtm).set_ivars(Ivars {
        ui: RefCell::new(Ui::default()),
        receiver,
        emit,
    });
    let delegate: Retained<Delegate> = unsafe { msg_send![super(this), init] };
    DELEGATE.store(Retained::as_ptr(&delegate).cast_mut(), Ordering::SeqCst);
    let application = NSApplication::sharedApplication(mtm);
    application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    application.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    application.run();
    DELEGATE.store(std::ptr::null_mut(), Ordering::SeqCst);
    Ok(())
}
