use crate::{
    api::Group,
    config::{self, Settings},
    engine::{self, Emit, Event, Run},
    update::{self, Release},
};
use anyhow::{Context, Result, ensure};
use dispatch2::{DispatchQueue, DispatchTime};
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
use objc2_quartz_core::kCACornerCurveContinuous;
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
    time::{Duration, SystemTime},
};

const UPDATE_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// Settings window size, and the vertical centre of its footer buttons.
const WINDOW: NSSize = NSSize::new(460.0, 500.0);
const FOOTER_MIDLINE: f64 = 470.0;
static DELEGATE: AtomicPtr<Delegate> = AtomicPtr::new(std::ptr::null_mut());
#[derive(PartialEq, Eq)]
enum Phase {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}
enum Update {
    Idle,
    Checking,
    Available(Release),
    Downloading(Release),
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
    CheckUpdate,
    InstallUpdate,
    AutoUpdate,
    Mode(String),
    Select(String, String),
}
struct Window {
    native: Retained<NSWindow>,
    /// Profile counts, parsed when the window opens and after an import, not on every render.
    summary: Option<String>,
    profile: Retained<NSTextField>,
    stats: Retained<NSTextField>,
    import: Retained<NSButton>,
    reveal: Retained<NSButton>,
    tun: Retained<NSButton>,
    tun_hint: Retained<NSTextField>,
    ports: Vec<Retained<NSTextField>>,
    login: Retained<NSButton>,
    auto: Retained<NSButton>,
    hint: Retained<NSTextField>,
    auto_update: Retained<NSButton>,
    update: Retained<NSButton>,
    dot: Retained<Surface>,
    status: Retained<NSTextField>,
    logs: Retained<NSButton>,
    start: Retained<NSButton>,
}
struct Ui {
    settings: Settings,
    phase: Phase,
    message: String,
    groups: Vec<Group>,
    run: Option<Arc<Run>>,
    log: Option<PathBuf>,
    status_item: Option<Retained<NSStatusItem>>,
    /// The 策略组与节点 submenu and the groups it was built from.
    groups_menu: Option<(Retained<NSMenu>, Vec<Group>)>,
    window: Option<Window>,
    refreshing: bool,
    operation: bool,
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
            status_item: None,
            groups_menu: None,
            window: None,
            refreshing: false,
            operation: false,
            update: Update::Idle,
            announced: None,
            checked: None,
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
        #[unsafe(method(menuNeedsUpdate:))]
        fn needs_update(&self, menu: &NSMenu) { self.fill_menu(menu); }
        #[unsafe(method(menuWillOpen:))]
        fn will_open(&self, _menu: &NSMenu) { self.refresh_groups(); }
    }
    unsafe impl NSWindowDelegate for Delegate {
        #[unsafe(method(windowWillClose:))]
        fn close_settings(&self, _notification: &NSNotification) { self.close_window(); }
        /// Picks up a login item approved in System Settings while the window was in the background.
        #[unsafe(method(windowDidBecomeKey:))]
        fn settings_key(&self, _notification: &NSNotification) { self.render(); }
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
        #[unsafe(method(openLogs:))]
        fn logs_button(&self, _sender: &AnyObject) { if let Err(error) = self.act(Action::Logs) { self.error(&error.to_string()); } }
        #[unsafe(method(updateClicked:))]
        fn update_button(&self, _sender: &AnyObject) {
            let action = update_entry(&self.ivars().ui.borrow().update).1;
            if let Some(action) = action { if let Err(error) = self.act(action) { self.error(&error.to_string()); } }
        }
        #[unsafe(method(showSettingsNotification:))]
        fn settings_notification(&self, _notification: &NSNotification) { self.show_settings(); }
    }
);

struct Fill {
    color: RefCell<Retained<NSColor>>,
    radius: f64,
    border: bool,
}
define_class!(
    /// A flat fill: the window background, a section card or the status dot. On screen it
    /// only sets layer properties, so it owns no bitmap; drawRect: covers --snapshot.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Fill]
    struct Surface;
    unsafe impl NSObjectProtocol for Surface {}
    impl Surface {
        #[unsafe(method(isFlipped))]
        fn flipped(&self) -> bool { true }
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool { true }
        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            let (Some(layer), fill) = (self.layer(), self.ivars()) else { return };
            // Resolving CGColor here follows the current light or dark appearance.
            layer.setBackgroundColor(Some(&fill.color.borrow().CGColor()));
            layer.setCornerRadius(fill.radius);
            if fill.border {
                layer.setCornerCurve(unsafe { kCACornerCurveContinuous });
                layer.setBorderWidth(self.hairline());
                layer.setBorderColor(Some(&NSColor::separatorColor().CGColor()));
            }
        }
        #[unsafe(method(drawRect:))]
        fn draw(&self, _rect: NSRect) {
            let fill = self.ivars();
            let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                self.bounds(), fill.radius, fill.radius,
            );
            fill.color.borrow().setFill();
            path.fill();
            if fill.border {
                NSColor::separatorColor().setStroke();
                // The bounds clip the outer half of the stroke.
                path.setLineWidth(self.hairline() * 2.0);
                path.stroke();
            }
        }
    }
);
impl Surface {
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        color: Retained<NSColor>,
        radius: f64,
        border: bool,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Fill {
            color: RefCell::new(color),
            radius,
            border,
        });
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
        view
    }
    fn set_color(&self, color: Retained<NSColor>) {
        *self.ivars().color.borrow_mut() = color;
        self.setNeedsDisplay(true);
    }
    fn hairline(&self) -> f64 {
        1.0 / self
            .window()
            .map_or(2.0, |window| window.backingScaleFactor())
    }
}

impl Delegate {
    fn setup(&self) {
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(24.0);
        // Items are built in menuNeedsUpdate:, so an open menu is never rebuilt under the cursor.
        let menu = NSMenu::new(self.mtm());
        menu.setAutoenablesItems(false);
        menu.setDelegate(Some(ProtocolObject::from_ref(self)));
        item.setMenu(Some(&menu));
        self.ivars().ui.borrow_mut().status_item = Some(item);
        self.install_main_menu();
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
        if !arguments.iter().any(|a| a == "--snapshot") {
            update_ticks(self.ivars().emit.clone(), Duration::from_secs(10));
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
        let Some(window) = &ui.window else {
            return;
        };
        let stopped = matches!(ui.phase, Phase::Stopped | Phase::Failed);
        let login = login_status();
        let text = |value: &str| NSString::from_str(value);
        let imported = window.summary.is_some();
        let name = text(if imported {
            &ui.settings.profile_name
        } else {
            "尚未导入配置"
        });
        window.profile.setStringValue(&name);
        // The name may be truncated.
        window.profile.setToolTip(imported.then_some(&*name));
        window.stats.setStringValue(&text(
            window
                .summary
                .as_deref()
                .unwrap_or("导入完整的 mihomo YAML 配置后即可连接。"),
        ));
        window.import.setEnabled(stopped);
        window.reveal.setEnabled(imported);
        window.tun.setState(i64::from(ui.settings.tun) as _);
        window.tun.setEnabled(stopped);
        window.tun_hint.setStringValue(&text(if ui.settings.tun {
            "接管所有应用的流量；连接时需要管理员授权。"
        } else {
            "只开放下方的本地 HTTP/SOCKS 端口，不修改系统代理，需在应用中手动指定。"
        }));
        let locked = (!stopped).then(|| text("停止连接后可修改端口。"));
        for (field, port) in window.ports.iter().zip([
            ui.settings.proxy_port,
            ui.settings.controller_port,
            ui.settings.dns_port,
        ]) {
            if field.currentEditor().is_none() {
                field.setStringValue(&text(&port.to_string()));
            }
            field.setEnabled(stopped);
            field.setToolTip(locked.as_deref());
        }
        window
            .login
            .setState(i64::from(login == SMAppServiceStatus::Enabled) as _);
        window
            .auto
            .setState(i64::from(ui.settings.auto_connect) as _);
        let approval = login == SMAppServiceStatus::RequiresApproval;
        window.hint.setStringValue(&text(if approval {
            "需要在「系统设置 → 通用 → 登录项」中允许 FlowBar。"
        } else {
            "登录自启与自动连接分别设置；使用 TUN 时，登录后自动连接仍可能弹出管理员授权。"
        }));
        let color = if approval {
            NSColor::systemOrangeColor()
        } else {
            NSColor::secondaryLabelColor()
        };
        window.hint.setTextColor(Some(&color));
        window
            .auto_update
            .setState(i64::from(ui.settings.auto_update) as _);
        let (title, action) = update_entry(&ui.update);
        window.update.setTitle(&text(&title));
        window.update.setEnabled(action.is_some());
        window.dot.set_color(match ui.phase {
            Phase::Running => NSColor::systemGreenColor(),
            Phase::Starting | Phase::Stopping => NSColor::systemOrangeColor(),
            Phase::Failed => NSColor::systemRedColor(),
            Phase::Stopped => NSColor::tertiaryLabelColor(),
        });
        let message = text(&ui.message);
        window.status.setStringValue(&message);
        window.status.setToolTip(Some(&message));
        let color = if ui.phase == Phase::Failed {
            NSColor::systemRedColor()
        } else {
            NSColor::labelColor()
        };
        window.status.setTextColor(Some(&color));
        // Centre one or two lines of status on the footer buttons, the dot on the first line.
        let frame = window.status.frame();
        let height = window
            .status
            .sizeThatFits(NSSize::new(frame.size.width, 100.0))
            .height
            .clamp(16.0, 34.0);
        let top = FOOTER_MIDLINE - height / 2.0;
        window
            .status
            .setFrame(rect(frame.origin.x, top, frame.size.width, height));
        window
            .dot
            .setFrameOrigin(NSPoint::new(20.0, top + height.min(17.0) / 2.0 - 4.0));
        window.logs.setEnabled(ui.run.is_some() || ui.log.is_some());
        window.start.setTitle(&text(if busy {
            "处理中…"
        } else if ui.phase == Phase::Running {
            "停止连接"
        } else {
            "启动连接"
        }));
        window.start.setEnabled(!busy);
    }
    /// Called by menuNeedsUpdate: just before the status menu opens.
    fn fill_menu(&self, menu: &NSMenu) {
        let ui = self.ivars().ui.borrow();
        let mtm = self.mtm();
        let busy = matches!(ui.phase, Phase::Starting | Phase::Stopping);
        let enabled = !busy && !ui.operation;
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
        let mut groups_menu = None;
        if !ui.groups.is_empty() {
            let groups = NSMenu::new(mtm);
            groups.setAutoenablesItems(false);
            self.fill_groups(&groups, &ui.groups, enabled);
            let parent = self.item("策略组与节点", None, true, false);
            parent.setSubmenu(Some(&groups));
            menu.addItem(&parent);
            groups_menu = Some((groups, ui.groups.clone()));
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
        let (title, action) = update_entry(&ui.update);
        let available = action.is_some();
        menu.addItem(&self.item(&title, action, available, false));
        menu.addItem(&self.item(
            "自动检查更新",
            Some(Action::AutoUpdate),
            true,
            ui.settings.auto_update,
        ));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let quit = self.item("退出 FlowBar", Some(Action::Quit), true, false);
        quit.setKeyEquivalent(&NSString::from_str("q"));
        menu.addItem(&quit);
        drop(ui);
        self.ivars().ui.borrow_mut().groups_menu = groups_menu;
    }
    fn fill_groups(&self, menu: &NSMenu, groups: &[Group], enabled: bool) {
        menu.removeAllItems();
        for group in groups {
            let choices = NSMenu::new(self.mtm());
            choices.setAutoenablesItems(false);
            for choice in &group.choices {
                choices.addItem(&self.item(
                    choice,
                    Some(Action::Select(group.name.clone(), choice.clone())),
                    enabled && group.kind.eq_ignore_ascii_case("selector"),
                    &group.current == choice,
                ));
            }
            let item = self.item(&group_title(group), None, true, false);
            item.setSubmenu(Some(&choices));
            menu.addItem(&item);
        }
    }
    /// Shows groups fetched after the menu opened. Groups with the same names and choices
    /// are updated in place, so a submenu the user is browsing stays open.
    fn update_groups(&self) {
        let mut ui = self.ivars().ui.borrow_mut();
        let enabled = !matches!(ui.phase, Phase::Starting | Phase::Stopping) && !ui.operation;
        let groups = ui.groups.clone();
        let Some((menu, shown)) = ui.groups_menu.as_mut() else {
            return;
        };
        let same_shape = shown.len() == groups.len()
            && shown.iter().zip(&groups).all(|(old, new)| {
                old.name == new.name && old.kind == new.kind && old.choices == new.choices
            });
        if !same_shape {
            self.fill_groups(menu, &groups, enabled);
        } else {
            for (index, group) in groups.iter().enumerate() {
                let Some(item) = menu.itemAtIndex(index as isize) else {
                    continue;
                };
                item.setTitle(&NSString::from_str(&group_title(group)));
                let Some(choices) = item.submenu() else {
                    continue;
                };
                for (position, choice) in group.choices.iter().enumerate() {
                    if let Some(entry) = choices.itemAtIndex(position as isize) {
                        entry.setEnabled(enabled && group.kind.eq_ignore_ascii_case("selector"));
                        entry.setState(i64::from(&group.current == choice) as _);
                    }
                }
            }
        }
        *shown = groups;
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
            Action::CheckUpdate => self.check_update(true),
            Action::InstallUpdate => {
                let release = match &self.ivars().ui.borrow().update {
                    Update::Available(release) => Some(release.clone()),
                    _ => None,
                };
                if let Some(release) = release {
                    self.offer_update(&release);
                }
            }
            Action::Tun | Action::Auto | Action::AutoUpdate => {
                let mut updated = self.ivars().ui.borrow().settings.clone();
                match action {
                    Action::Tun => updated.tun = !updated.tun,
                    Action::Auto => updated.auto_connect = !updated.auto_connect,
                    _ => updated.auto_update = !updated.auto_update,
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
        let mut groups_changed = false;
        let mut errors = vec![];
        let mut notices = vec![];
        let (mut offer, mut staged, mut due) = (None, None, false);
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
                            Ok(Some(release)) => {
                                ui.checked = Some(SystemTime::now());
                                if manual || ui.announced.as_deref() != Some(release.version()) {
                                    ui.announced = Some(release.version().into());
                                    offer = Some(release.clone());
                                }
                                ui.update = Update::Available(release);
                            }
                            Ok(None) => {
                                ui.checked = Some(SystemTime::now());
                                if manual {
                                    notices.push(format!("已是最新版本（{}）。", update::CURRENT));
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
            self.render();
        }
        if groups_changed {
            self.update_groups();
        }
        // Alerts run a modal loop that can re-enter drain; showing them mid-batch would
        // apply this batch's older events after newer ones.
        for error in errors {
            self.error(&error.to_string());
        }
        for notice in notices {
            self.alert("FlowBar", &notice, &[]);
        }
        if let Some(release) = offer {
            self.offer_update(&release);
        }
        if let Some(staging) = staged {
            self.install_update(&staging);
        }
        if due {
            self.update_due();
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
                updated.auto_update = window.auto_update.state() == 1;
                if matches!(ui.phase, Phase::Stopped | Phase::Failed) {
                    updated.tun = window.tun.state() == 1;
                    let ports = window
                        .ports
                        .iter()
                        .map(|f| f.stringValue().to_string().trim().parse::<u16>())
                        .collect::<std::result::Result<Vec<_>, _>>()
                        .context("端口须为 1024–65535 之间的整数。")?;
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
                let mut ui = self.ivars().ui.borrow_mut();
                ui.settings = settings;
                if let Some(window) = &mut ui.window {
                    window.summary = profile_summary();
                }
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
        let created = self.ivars().ui.borrow().window.is_none();
        if created {
            self.make_window();
        }
        self.render();
        // Window callbacks borrow the state, so none is held while showing it.
        let native = self
            .ivars()
            .ui
            .borrow()
            .window
            .as_ref()
            .map(|w| w.native.clone());
        if let Some(native) = native {
            if created {
                native.center();
            }
            native.makeKeyAndOrderFront(None);
            if created {
                // Opening the window should not start editing the first port field.
                native.makeFirstResponder(None);
            }
        }
        #[allow(deprecated)]
        NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
    }
    fn close_window(&self) {
        let native = self
            .ivars()
            .ui
            .borrow()
            .window
            .as_ref()
            .map(|w| w.native.clone());
        // Ending editing commits a port that was typed but not confirmed with Return.
        if let Some(native) = native {
            native.makeFirstResponder(None);
        }
        self.ivars().ui.borrow_mut().window = None;
    }
    fn make_window(&self) {
        let mtm = self.mtm();
        let native = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), WINDOW),
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
        native.setAutorecalculatesKeyViewLoop(true);
        // Flipped, so the frames below run top to bottom.
        let view = Surface::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), WINDOW),
            NSColor::windowBackgroundColor(),
            0.0,
            false,
        );
        view.setWantsLayer(true);
        native.setContentView(Some(&view));
        let (regular, semibold) = unsafe { (NSFontWeightRegular, NSFontWeightSemibold) };
        let small = NSFont::systemFontOfSize(11.0);
        let caption = NSFont::systemFontOfSize_weight(12.0, semibold);
        let section = |title: &str, y: f64, height: f64| {
            self.label(&view, title, &caption, true, 1, rect(34.0, y, 200.0, 16.0));
            let fill = NSColor::labelColor().colorWithAlphaComponent(0.05);
            view.addSubview(&Surface::new(
                mtm,
                rect(20.0, y + 20.0, 420.0, height),
                fill,
                10.0,
                true,
            ));
        };
        let add = |button: Retained<NSButton>| {
            view.addSubview(&button);
            button
        };

        section("配置", 16.0, 72.0);
        let profile = self.label(
            &view,
            "",
            &NSFont::systemFontOfSize_weight(13.0, semibold),
            false,
            1,
            rect(34.0, 51.0, 168.0, 18.0),
        );
        let import = add(self.button(
            "导入 YAML…",
            sel!(importYaml:),
            rect(204.0, 44.0, 108.0, 32.0),
            false,
        ));
        let reveal = add(self.button(
            "在访达中显示",
            sel!(revealProfile:),
            rect(308.0, 44.0, 124.0, 32.0),
            false,
        ));
        let stats = self.label(&view, "", &small, true, 1, rect(34.0, 78.0, 392.0, 16.0));

        section("连接", 126.0, 122.0);
        let tun = add(self.button(
            "使用 TUN 接管流量",
            sel!(optionsChanged:),
            rect(34.0, 158.0, 392.0, 18.0),
            true,
        ));
        let tun_hint = self.label(&view, "", &small, true, 2, rect(54.0, 180.0, 372.0, 28.0));
        let digits = NSFont::monospacedDigitSystemFontOfSize_weight(13.0, regular);
        let mut ports = vec![];
        for (index, title) in ["HTTP/SOCKS", "控制接口", "DNS"].iter().enumerate() {
            let x = 34.0 + index as f64 * 135.0;
            self.label(&view, title, &small, true, 1, rect(x, 214.0, 122.0, 14.0));
            let field =
                NSTextField::initWithFrame(NSTextField::alloc(mtm), rect(x, 230.0, 122.0, 22.0));
            field.setFont(Some(&digits));
            unsafe {
                field.setTarget(Some(self));
                field.setAction(Some(sel!(optionsChanged:)));
            }
            // Tabbing or clicking away saves too, not only Return.
            if let Some(cell) = field.cell() {
                cell.setSendsActionOnEndEditing(true);
            }
            view.addSubview(&field);
            ports.push(field);
        }

        section("启动与更新", 286.0, 128.0);
        let version = self.label(
            &view,
            &format!("版本 {}", update::CURRENT),
            &small,
            true,
            1,
            rect(226.0, 287.0, 200.0, 16.0),
        );
        version.setAlignment(NSTextAlignment::Right);
        let login = add(self.button(
            "登录时打开 FlowBar",
            sel!(loginChanged:),
            rect(34.0, 318.0, 392.0, 18.0),
            true,
        ));
        let auto = add(self.button(
            "打开 FlowBar 后自动连接",
            sel!(optionsChanged:),
            rect(34.0, 344.0, 392.0, 18.0),
            true,
        ));
        let hint = self.label(&view, "", &small, true, 2, rect(54.0, 366.0, 372.0, 28.0));
        let auto_update = add(self.button(
            "自动检查更新",
            sel!(optionsChanged:),
            rect(34.0, 402.0, 250.0, 18.0),
            true,
        ));
        let update = add(self.button(
            "检查更新…",
            sel!(updateClicked:),
            rect(292.0, 395.0, 140.0, 32.0),
            false,
        ));

        let dot = Surface::new(
            mtm,
            rect(20.0, FOOTER_MIDLINE - 4.0, 8.0, 8.0),
            NSColor::tertiaryLabelColor(),
            4.0,
            false,
        );
        view.addSubview(&dot);
        // render() sets the height and position; the text stays selectable so errors can be copied.
        let status = self.label(
            &view,
            "",
            &NSFont::systemFontOfSize(12.0),
            false,
            2,
            rect(34.0, FOOTER_MIDLINE - 8.0, 194.0, 16.0),
        );
        status.setSelectable(true);
        let logs = add(self.button(
            "查看日志",
            sel!(openLogs:),
            rect(230.0, 454.0, 94.0, 32.0),
            false,
        ));
        let start = add(self.button(
            "启动连接",
            sel!(toggleConnection:),
            rect(320.0, 454.0, 112.0, 32.0),
            false,
        ));
        self.ivars().ui.borrow_mut().window = Some(Window {
            native,
            summary: profile_summary(),
            profile,
            stats,
            import,
            reveal,
            tun,
            tun_hint,
            ports,
            login,
            auto,
            hint,
            auto_update,
            update,
            dot,
            status,
            logs,
            start,
        });
    }
    fn label(
        &self,
        view: &NSView,
        title: &str,
        font: &NSFont,
        secondary: bool,
        lines: isize,
        frame: NSRect,
    ) -> Retained<NSTextField> {
        let field = NSTextField::wrappingLabelWithString(&NSString::from_str(title), self.mtm());
        field.setFont(Some(font));
        field.setFrame(frame);
        field.setSelectable(false);
        field.setMaximumNumberOfLines(lines);
        if lines == 1 {
            field.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        } else if let Some(cell) = field.cell() {
            cell.setTruncatesLastVisibleLine(true);
        }
        if secondary {
            field.setTextColor(Some(&NSColor::secondaryLabelColor()));
        }
        view.addSubview(&field);
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
    /// An accessory app has no menu bar, but its main menu still provides the standard
    /// shortcuts: ⌘W and ⌘Q for the settings window, and editing keys in the port fields.
    fn install_main_menu(&self) {
        let mtm = self.mtm();
        let bar = NSMenu::new(mtm);
        let groups: [&[(&str, objc2::runtime::Sel, &str)]; 2] = [
            &[
                ("关闭窗口", sel!(performClose:), "w"),
                ("退出 FlowBar", sel!(terminate:), "q"),
            ],
            &[
                ("撤销", sel!(undo:), "z"),
                ("重做", sel!(redo:), "Z"),
                ("剪切", sel!(cut:), "x"),
                ("拷贝", sel!(copy:), "c"),
                ("粘贴", sel!(paste:), "v"),
                ("全选", sel!(selectAll:), "a"),
            ],
        ];
        for group in groups {
            let menu = NSMenu::new(mtm);
            for (title, action, key) in group {
                // No target: the action goes to the key window's responder chain.
                let item = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str(title),
                        Some(*action),
                        &NSString::from_str(key),
                    )
                };
                menu.addItem(&item);
            }
            let parent = NSMenuItem::new(mtm);
            parent.setSubmenu(Some(&menu));
            bar.addItem(&parent);
        }
        NSApplication::sharedApplication(mtm).setMainMenu(Some(&bar));
    }
    /// Local proxy for update requests while it runs; TUN already routes them.
    fn update_proxy(&self) -> Option<u16> {
        let ui = self.ivars().ui.borrow();
        ui.run
            .as_ref()
            .filter(|run| ui.phase == Phase::Running && !run.settings.tun)
            .map(|run| run.settings.proxy_port)
    }
    fn update_due(&self) {
        let due = {
            let ui = self.ivars().ui.borrow();
            ui.settings.auto_update
                && matches!(ui.update, Update::Idle)
                && ui.checked.is_none_or(|time| {
                    time.elapsed()
                        .map_or(true, |elapsed| elapsed >= UPDATE_INTERVAL)
                })
        };
        if due {
            self.check_update(false);
        }
    }
    fn check_update(&self, manual: bool) {
        let proxy = self.update_proxy();
        {
            let mut ui = self.ivars().ui.borrow_mut();
            if !matches!(ui.update, Update::Idle) {
                return;
            }
            ui.update = Update::Checking;
        }
        self.render();
        let emit = self.ivars().emit.clone();
        thread::spawn(move || {
            let result = update::check(proxy).map_err(|e| format!("{e:#}"));
            emit(Event::Update(update::Event::Checked(manual, result)));
        });
    }
    fn offer_update(&self, release: &Release) {
        let body = release.body.as_deref().unwrap_or_default().trim();
        let mut notes: String = body.chars().take(600).collect();
        if notes.len() < body.len() {
            notes.push('…');
        }
        let choice = self.alert(
            &format!("FlowBar {} 已发布", release.version()),
            format!(
                "当前版本 {}。安装时会断开连接，完成后自动重新打开 FlowBar。\n\n{notes}",
                update::CURRENT
            )
            .trim_end(),
            &["安装并重启", "稍后", "查看发布页"],
        );
        match choice {
            0 => self.download_update(),
            2 if release.html_url.starts_with("https://github.com/") => {
                open(Path::new(&release.html_url), false)
            }
            _ => {}
        }
    }
    fn download_update(&self) {
        let proxy = self.update_proxy();
        let release = {
            let mut ui = self.ivars().ui.borrow_mut();
            let Update::Available(release) = &ui.update else {
                return;
            };
            let release = release.clone();
            ui.update = Update::Downloading(release.clone());
            release
        };
        self.render();
        let emit = self.ivars().emit.clone();
        thread::spawn(move || {
            let result = update::stage(&release, proxy).map_err(|e| format!("{e:#}"));
            emit(Event::Update(update::Event::Staged(result)));
        });
    }
    fn install_update(&self, staging: &Path) {
        match update::install(staging) {
            // Quitting stops the connection; the relaunch waits for this process to exit.
            Ok(()) => NSApplication::sharedApplication(self.mtm()).terminate(None),
            Err(error) => self.error(&format!("更新失败：{error:#}")),
        }
    }
    fn error(&self, message: &str) {
        self.alert("FlowBar", message, &[]);
    }
    /// Returns the index of the clicked button.
    fn alert(&self, title: &str, message: &str, buttons: &[&str]) -> isize {
        // Results of background work can arrive while another app is in front.
        #[allow(deprecated)]
        NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(message));
        for button in buttons {
            alert.addButtonWithTitle(&NSString::from_str(button));
        }
        alert.runModal() - NSAlertFirstButtonReturn
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

/// Menu and settings window text for the update state, and the action it offers.
fn update_entry(update: &Update) -> (String, Option<Action>) {
    match update {
        Update::Idle => ("检查更新…".into(), Some(Action::CheckUpdate)),
        Update::Checking => ("正在检查更新…".into(), None),
        Update::Available(release) => (
            format!("安装更新 {}…", release.version()),
            Some(Action::InstallUpdate),
        ),
        Update::Downloading(release) => (format!("正在下载 {}…", release.version()), None),
    }
}
/// Emits an update tick after `delay` and then hourly from the main queue, without keeping
/// a sleeping thread. Hourly ticks keep the daily check on time across system sleep.
fn update_ticks(emit: Emit, delay: Duration) {
    let when = DispatchTime::try_from(delay).unwrap_or(DispatchTime::FOREVER);
    let _ = DispatchQueue::main().after(when, move || {
        emit(Event::Update(update::Event::Due));
        update_ticks(emit, Duration::from_secs(3600));
    });
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
fn group_title(group: &Group) -> String {
    format!("{} → {}", group.name, group.current)
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
