use super::{Action, App, Entry, Form, Login, Phase, Shell};
use crate::{
    api::Group,
    config,
    engine::{Emit, Event},
    update,
};
use anyhow::{Result, ensure};
use core_foundation::{base::TCFType, runloop::*};
use dispatch2::{DispatchQueue, DispatchTime};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    sel,
};
use objc2_app_kit::*;
use objc2_foundation::{
    NSArray, NSAttributedString, NSDictionary, NSDistributedNotificationCenter, NSNotification,
    NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use objc2_quartz_core::kCACornerCurveContinuous;
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use std::sync::Arc;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicPtr, Ordering},
    time::Duration,
};

pub const LOGIN_HINT: &str =
    "登录自启与自动连接分别设置；使用 TUN 时，登录后自动连接仍可能弹出管理员授权。";
/// Settings window size, and the vertical centre of its footer buttons.
const WINDOW: NSSize = NSSize::new(460.0, 500.0);
const FOOTER_MIDLINE: f64 = 470.0;
static DELEGATE: AtomicPtr<Delegate> = AtomicPtr::new(std::ptr::null_mut());
struct Window {
    native: Retained<NSWindow>,
    profiles: Retained<NSPopUpButton>,
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
#[derive(Default)]
struct Mac {
    status_item: Option<Retained<NSStatusItem>>,
    /// The 策略组与节点 submenu and the groups it was built from.
    groups_menu: Option<(Retained<NSMenu>, Vec<Group>)>,
    window: Option<Window>,
    menu_probe: Option<MenuProbe>,
}
struct MenuProbe {
    menu: Retained<NSMenu>,
    button: Retained<DelayButton>,
    clicked: bool,
    passed: bool,
    deadline: std::time::Instant,
}
struct Ivars {
    app: App,
    mac: RefCell<Mac>,
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
            self.ivars().app.stop();
            NSApplicationTerminateReply::TerminateNow
        }
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn reopen(&self, _application: &NSApplication, _visible: bool) -> bool { self.show_settings(); true }
    }
    unsafe impl NSMenuDelegate for Delegate {
        #[unsafe(method(menuNeedsUpdate:))]
        fn needs_update(&self, menu: &NSMenu) { self.fill_menu(menu); }
        #[unsafe(method(menuWillOpen:))]
        fn will_open(&self, _menu: &NSMenu) { self.ivars().app.refresh_groups(); }
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
            if let Some(action) = action { self.perform(action); }
        }
        #[unsafe(method(delayClicked:))]
        fn delay_clicked(&self, sender: &DelayButton) {
            self.perform(Action::Delay(sender.ivars().clone()));
        }
        #[unsafe(method(toggleConnection:))]
        fn connection_button(&self, _sender: &AnyObject) { self.perform(Action::Connect); }
        #[unsafe(method(importYaml:))]
        fn import_button(&self, _sender: &AnyObject) { self.perform(Action::Import); }
        #[unsafe(method(profileChosen:))]
        fn profile_list(&self, sender: &NSPopUpButton) {
            if let Some(name) = sender.titleOfSelectedItem() { self.perform(Action::Profile(name.to_string())); }
        }
        #[unsafe(method(optionsChanged:))]
        fn options_button(&self, _sender: &AnyObject) {
            let app = &self.ivars().app;
            if let Err(error) = app.save_options(self) { app.error(self, &error.to_string()); self.render(); }
        }
        #[unsafe(method(loginChanged:))]
        fn login_button(&self, _sender: &AnyObject) { self.perform(Action::Login); }
        #[unsafe(method(revealProfile:))]
        fn reveal_button(&self, _sender: &AnyObject) { self.perform(Action::Reveal); }
        #[unsafe(method(openLogs:))]
        fn logs_button(&self, _sender: &AnyObject) { self.perform(Action::Logs); }
        #[unsafe(method(updateClicked:))]
        fn update_button(&self, _sender: &AnyObject) {
            if let Some(action) = self.ivars().app.update_action() { self.perform(action); }
        }
        #[unsafe(method(showSettingsNotification:))]
        fn settings_notification(&self, _notification: &NSNotification) { self.show_settings(); }
    }
);

// A view-backed command receives the click without NSMenu dismissing its menu tree.
define_class!(
    #[unsafe(super(NSButton, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = String]
    struct DelayButton;
    unsafe impl NSObjectProtocol for DelayButton {}
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

impl Shell for Delegate {
    fn smoke_delay(&self, group: &str) -> bool {
        let mtm = self.mtm();
        let groups_menu = NSMenu::new(mtm);
        groups_menu.setAutoenablesItems(false);
        self.add_entries(&groups_menu, self.ivars().app.group_entries(), &mut None);
        let groups = self.ivars().app.ui.borrow().groups.clone();
        let index = groups.iter().position(|g| g.name == group).unwrap();
        let menu = groups_menu
            .itemAtIndex(index as isize)
            .unwrap()
            .submenu()
            .unwrap();
        let button = menu
            .itemAtIndex(0)
            .unwrap()
            .view()
            .unwrap()
            .downcast::<DelayButton>()
            .unwrap();
        self.ivars().mac.borrow_mut().groups_menu = Some((groups_menu, groups));
        self.ivars().app.ui.borrow_mut().smoke_step = None;
        self.ivars().mac.borrow_mut().menu_probe = Some(MenuProbe {
            menu: menu.clone(),
            button,
            clicked: false,
            passed: false,
            deadline: std::time::Instant::now() + Duration::from_secs(15),
        });
        extern "C" fn tick(_: CFRunLoopTimerRef, _: *mut std::ffi::c_void) {
            let delegate = unsafe { &*DELEGATE.load(Ordering::SeqCst) };
            let (button, clicked, deadline) = {
                let mac = delegate.ivars().mac.borrow();
                let probe = mac.menu_probe.as_ref().unwrap();
                (probe.button.clone(), probe.clicked, probe.deadline)
            };
            assert!(
                std::time::Instant::now() < deadline,
                "latency menu smoke timed out"
            );
            assert!(
                button.window().is_some_and(|w| w.isVisible()),
                "latency click dismissed its menu"
            );
            if !clicked {
                delegate
                    .ivars()
                    .mac
                    .borrow_mut()
                    .menu_probe
                    .as_mut()
                    .unwrap()
                    .clicked = true;
                unsafe {
                    button.performClick(None);
                }
                assert!(!button.isEnabled());
            } else if delegate.ivars().app.ui.borrow().testing.is_empty() {
                assert_eq!(delegate.ivars().app.ui.borrow().delays.len(), 2);
                assert!(button.isEnabled());
                let menu = {
                    let mut mac = delegate.ivars().mac.borrow_mut();
                    let probe = mac.menu_probe.as_mut().unwrap();
                    probe.passed = true;
                    probe.menu.clone()
                };
                // Result titles have been refreshed while the real menu is tracking.
                assert!(menu.itemAtIndex(2).unwrap().attributedTitle().is_some());
                menu.cancelTracking();
            }
        }
        let timer = CFRunLoopTimer::new(
            unsafe { core_foundation::date::CFAbsoluteTimeGetCurrent() } + 0.1,
            0.05,
            0,
            0,
            tick,
            std::ptr::null_mut(),
        );
        unsafe {
            CFRunLoopAddTimer(
                CFRunLoopGetMain(),
                timer.as_concrete_TypeRef(),
                kCFRunLoopCommonModes,
            );
        }
        menu.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(400.0, 400.0), None);
        unsafe {
            CFRunLoopTimerInvalidate(timer.as_concrete_TypeRef());
        }
        assert!(
            self.ivars()
                .mac
                .borrow_mut()
                .menu_probe
                .take()
                .unwrap()
                .passed
        );
        println!(
            "PASS native macOS latency button keeps its menu visible through asynchronous results"
        );
        self.ivars().app.ui.borrow_mut().smoke_step = Some(5);
        self.ivars().app.advance_smoke(self);
        true
    }
    fn render(&self) {
        let view = self.ivars().app.view();
        let mac = self.ivars().mac.borrow();
        let mtm = self.mtm();
        if let Some(button) = mac.status_item.as_ref().and_then(|item| item.button(mtm)) {
            let symbol = if view.phase == Phase::Running {
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
                view.message
            ))));
        }
        let Some(window) = &mac.window else {
            return;
        };
        let text = |value: &str| NSString::from_str(value);
        let titles = window.profiles.itemTitles();
        let shown: Vec<_> = (0..titles.count())
            .map(|index| titles.objectAtIndex(index).to_string())
            .collect();
        if shown != view.profiles {
            window.profiles.removeAllItems();
            let names: Vec<_> = view.profiles.iter().map(|name| text(name)).collect();
            window
                .profiles
                .addItemsWithTitles(&NSArray::from_retained_slice(&names));
        }
        // An index of -1 selects nothing.
        window
            .profiles
            .selectItemAtIndex(view.profile_index.map_or(-1, |index| index as isize));
        window.profiles.setEnabled(view.switchable);
        // The name may be truncated.
        window.profiles.setToolTip(
            view.imported
                .then(|| window.profiles.titleOfSelectedItem())
                .flatten()
                .as_deref(),
        );
        window.stats.setStringValue(&text(&view.stats));
        window.import.setEnabled(view.editable);
        window.reveal.setEnabled(view.imported);
        window.tun.setState(i64::from(view.tun) as _);
        window.tun.setEnabled(view.editable);
        window.tun_hint.setStringValue(&text(view.tun_hint));
        let locked = view.ports_hint.map(text);
        for (field, port) in window.ports.iter().zip(view.ports) {
            if field.currentEditor().is_none() {
                field.setStringValue(&text(&port.to_string()));
            }
            field.setEnabled(view.editable);
            field.setToolTip(locked.as_deref());
        }
        window
            .login
            .setState(i64::from(view.login == Login::Enabled) as _);
        window.auto.setState(i64::from(view.auto_connect) as _);
        window.hint.setStringValue(&text(view.login_hint));
        let color = if view.login == Login::RequiresApproval {
            NSColor::systemOrangeColor()
        } else {
            NSColor::secondaryLabelColor()
        };
        window.hint.setTextColor(Some(&color));
        window
            .auto_update
            .setState(i64::from(view.auto_update) as _);
        window.update.setTitle(&text(&view.update_title));
        window.update.setEnabled(view.update_enabled);
        window.dot.set_color(match view.phase {
            Phase::Running => NSColor::systemGreenColor(),
            Phase::Starting | Phase::Stopping => NSColor::systemOrangeColor(),
            Phase::Failed => NSColor::systemRedColor(),
            Phase::Stopped => NSColor::tertiaryLabelColor(),
        });
        let message = text(&view.message);
        window.status.setStringValue(&message);
        window.status.setToolTip(Some(&message));
        let color = if view.phase == Phase::Failed {
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
        window.logs.setEnabled(view.logs);
        window.start.setTitle(&text(view.start_title));
        window.start.setEnabled(view.start_enabled);
    }
    fn show_settings(&self) {
        let created = self.ivars().mac.borrow().window.is_none();
        if created {
            self.make_window();
        }
        self.render();
        // Window callbacks borrow the state, so none is held while showing it.
        let native = self.native_window();
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
    fn close_settings(&self) {
        if let Some(window) = self.native_window() {
            window.close();
        }
    }
    fn settings_open(&self) -> bool {
        self.ivars().mac.borrow().window.is_some()
    }
    fn form(&self) -> Option<Form> {
        let mac = self.ivars().mac.borrow();
        let window = mac.window.as_ref()?;
        Some(Form {
            tun: window.tun.state() == 1,
            auto_connect: window.auto.state() == 1,
            auto_update: window.auto_update.state() == 1,
            ports: window
                .ports
                .iter()
                .map(|f| f.stringValue().to_string())
                .collect(),
        })
    }
    #[allow(deprecated)]
    fn choose_profile(&self) -> Option<PathBuf> {
        let panel = NSOpenPanel::openPanel(self.mtm());
        panel.setAllowsMultipleSelection(false);
        panel.setCanChooseDirectories(false);
        panel.setAllowedFileTypes(Some(&NSArray::from_retained_slice(&[
            NSString::from_str("yaml"),
            NSString::from_str("yml"),
        ])));
        if panel.runModal() != 1 {
            return None;
        }
        panel
            .URL()
            .and_then(|url| url.path())
            .map(|path| PathBuf::from(path.to_string()))
    }
    fn alert(&self, title: &str, message: &str, buttons: &[&str]) -> usize {
        // Results of background work can arrive while another app is in front.
        #[allow(deprecated)]
        NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(message));
        for button in buttons {
            alert.addButtonWithTitle(&NSString::from_str(button));
        }
        usize::try_from(alert.runModal() - NSAlertFirstButtonReturn).unwrap_or(buttons.len())
    }
    /// Shows groups fetched after the menu opened, and latencies measured meanwhile. Groups
    /// with the same names and choices are updated in place, so a submenu the user is
    /// browsing stays open.
    fn groups_changed(&self) {
        let groups = self.ivars().app.ui.borrow().groups.clone();
        let entries = self.ivars().app.group_entries();
        let mut mac = self.ivars().mac.borrow_mut();
        let Some((menu, shown)) = mac.groups_menu.as_mut() else {
            return;
        };
        let same_shape = shown.len() == groups.len()
            && shown.iter().zip(&groups).all(|(old, new)| {
                old.name == new.name && old.kind == new.kind && old.choices == new.choices
            });
        if !same_shape {
            menu.removeAllItems();
            self.add_entries(menu, entries, &mut None);
        } else {
            for (index, entry) in entries.into_iter().enumerate() {
                let (Some(item), Entry::Menu { title, entries, .. }) =
                    (menu.itemAtIndex(index as isize), entry)
                else {
                    continue;
                };
                item.setTitle(&NSString::from_str(&title));
                if let Some(submenu) = item.submenu() {
                    update_items(&submenu, &entries);
                }
            }
        }
        *shown = groups;
    }
    fn schedule_updates(&self) {
        update_ticks(self.ivars().app.emit.clone(), Duration::from_secs(10));
    }
    fn snapshot(&self, path: &Path) {
        let mac = self.ivars().mac.borrow();
        if let Some(window) = &mac.window {
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
    fn quit(&self) {
        NSApplication::sharedApplication(self.mtm()).terminate(None);
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
        self.ivars().mac.borrow_mut().status_item = Some(item);
        self.install_main_menu();
        unsafe {
            NSDistributedNotificationCenter::defaultCenter().addObserver_selector_name_object(
                self,
                sel!(showSettingsNotification:),
                Some(&NSString::from_str("dev.local.FlowBar.showSettings")),
                None,
            );
        }
        self.ivars().app.launch(self);
    }
    fn perform(&self, action: Action) {
        self.ivars().app.perform(self, action);
    }
    fn drain(&self) {
        self.ivars().app.drain(self);
    }
    fn native_window(&self) -> Option<Retained<NSWindow>> {
        let mac = self.ivars().mac.borrow();
        mac.window.as_ref().map(|w| w.native.clone())
    }
    /// Called by menuNeedsUpdate: just before the status menu opens.
    fn fill_menu(&self, menu: &NSMenu) {
        let entries = self.ivars().app.menu(login_status());
        menu.removeAllItems();
        let mut groups_menu = None;
        self.add_entries(menu, entries, &mut groups_menu);
        let groups = self.ivars().app.ui.borrow().groups.clone();
        self.ivars().mac.borrow_mut().groups_menu = groups_menu.map(|menu| (menu, groups));
    }
    fn add_entries(
        &self,
        menu: &NSMenu,
        entries: Vec<Entry>,
        groups_menu: &mut Option<Retained<NSMenu>>,
    ) {
        let mtm = self.mtm();
        let tab = tab_location(&entries);
        for entry in entries {
            match entry {
                Entry::Separator => menu.addItem(&NSMenuItem::separatorItem(mtm)),
                Entry::Item {
                    title,
                    detail,
                    action,
                    enabled,
                    checked,
                } => {
                    let delay_group = match &action {
                        Some(Action::Delay(group)) => Some(group.clone()),
                        _ => None,
                    };
                    let quit = matches!(action, Some(Action::Quit));
                    let item = self.item(&title, action, enabled, checked.unwrap_or(false));
                    if detail.is_some() {
                        set_title(&item, &title, detail.as_deref(), tab);
                    }
                    if quit {
                        item.setKeyEquivalent(&NSString::from_str("q"));
                    }
                    if let Some(group) = delay_group {
                        let button = DelayButton::alloc(mtm).set_ivars(group);
                        let button: Retained<DelayButton> = unsafe {
                            msg_send![super(button), initWithFrame: rect(0.0, 0.0, tab.max(190.0), 24.0)]
                        };
                        button.setTitle(&NSString::from_str(&format!("   {title}")));
                        button.setFont(Some(&NSFont::menuFontOfSize(0.0)));
                        button.setAlignment(NSTextAlignment::Left);
                        button.setBordered(false);
                        button.setEnabled(enabled);
                        button.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
                        unsafe {
                            button.setTarget(Some(self));
                            button.setAction(Some(sel!(delayClicked:)));
                        }
                        item.setView(Some(&button));
                    }
                    menu.addItem(&item);
                }
                Entry::Menu {
                    title,
                    entries,
                    groups,
                } => {
                    let submenu = NSMenu::new(mtm);
                    submenu.setAutoenablesItems(false);
                    self.add_entries(&submenu, entries, groups_menu);
                    let parent = self.item(&title, None, true, false);
                    parent.setSubmenu(Some(&submenu));
                    menu.addItem(&parent);
                    if groups {
                        *groups_menu = Some(submenu);
                    }
                }
            }
        }
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
    fn close_window(&self) {
        // Ending editing commits a port that was typed but not confirmed with Return.
        if let Some(native) = self.native_window() {
            native.makeFirstResponder(None);
        }
        self.ivars().mac.borrow_mut().window = None;
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
        let profiles = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            rect(31.0, 47.0, 170.0, 26.0),
            false,
        );
        unsafe {
            profiles.setTarget(Some(self));
            profiles.setAction(Some(sel!(profileChosen:)));
        }
        view.addSubview(&profiles);
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
        self.ivars().app.load_summary();
        self.ivars().mac.borrow_mut().window = Some(Window {
            native,
            profiles,
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
/// Updates the items of an open menu from entries of the same shape.
fn update_items(menu: &NSMenu, entries: &[Entry]) {
    let tab = tab_location(entries);
    for (index, entry) in entries.iter().enumerate() {
        let (
            Some(item),
            Entry::Item {
                title,
                detail,
                enabled,
                checked,
                ..
            },
        ) = (menu.itemAtIndex(index as isize), entry)
        else {
            continue;
        };
        set_title(&item, title, detail.as_deref(), tab);
        if let Some(view) = item
            .view()
            .and_then(|view| view.downcast::<DelayButton>().ok())
        {
            view.setTitle(&NSString::from_str(&format!("   {title}")));
            view.setEnabled(*enabled);
        }
        item.setEnabled(*enabled);
        item.setState(i64::from(checked.unwrap_or(false)) as _);
    }
}
/// A menu item's title, with `detail`, such as a node's latency, right-aligned at `tab`.
fn set_title(item: &NSMenuItem, title: &str, detail: Option<&str>, tab: f64) {
    let Some(detail) = detail else {
        item.setAttributedTitle(None);
        item.setTitle(&NSString::from_str(title));
        return;
    };
    let stop = unsafe {
        NSTextTab::initWithTextAlignment_location_options(
            NSTextTab::alloc(),
            NSTextAlignment::Right,
            tab,
            &NSDictionary::new(),
        )
    };
    let style = NSMutableParagraphStyle::new();
    style.setTabStops(Some(&NSArray::from_retained_slice(&[stop])));
    item.setAttributedTitle(Some(&menu_text(
        &format!("{title}\t{detail}"),
        Some(&style),
    )));
}
/// Where the details of a menu's items end: past its widest title and detail, and a gap.
fn tab_location(entries: &[Entry]) -> f64 {
    let width = |text: &str| menu_text(text, None).size().width;
    entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Item {
                title,
                detail: Some(detail),
                ..
            } => Some(width(title) + width(detail)),
            _ => None,
        })
        .fold(0.0, f64::max)
        + 24.0
}
/// Text in the menu font, which an attributed title does not use by default.
fn menu_text(text: &str, style: Option<&NSParagraphStyle>) -> Retained<NSAttributedString> {
    let font = NSFont::menuFontOfSize(0.0);
    let font: &AnyObject = &font;
    let (font_key, style_key) = unsafe { (NSFontAttributeName, NSParagraphStyleAttributeName) };
    let mut keys = vec![font_key];
    let mut values = vec![font];
    if let Some(style) = style {
        keys.push(style_key);
        values.push(style);
    }
    let attributes = NSDictionary::from_slices(&keys, &values);
    unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(text),
            Some(&attributes),
        )
    }
}
fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}
pub fn open(path: &Path, reveal: bool) {
    let mut command = Command::new("/usr/bin/open");
    if reveal {
        command.arg("-R");
    }
    let _ = command.arg(path).spawn();
}
pub fn open_url(url: &str) {
    open(Path::new(url), false);
}
pub fn login_status() -> Login {
    objc2::rc::autoreleasepool(
        |_| match unsafe { SMAppService::mainAppService().status() } {
            SMAppServiceStatus::Enabled => Login::Enabled,
            SMAppServiceStatus::RequiresApproval => Login::RequiresApproval,
            _ => Login::Disabled,
        },
    )
}
pub fn toggle_login() -> Result<()> {
    let service = unsafe { SMAppService::mainAppService() };
    unsafe {
        match service.status() {
            SMAppServiceStatus::Enabled => service
                .unregisterAndReturnError()
                .map_err(|e| anyhow::anyhow!(e.to_string()))?,
            SMAppServiceStatus::RequiresApproval => SMAppService::openSystemSettingsLoginItems(),
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
    Ok(())
}

pub fn login_roundtrip() -> Result<()> {
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

struct MenuWake(CFRunLoopSource);
// CFRunLoopSourceSignal and CFRunLoopWakeUp are thread-safe Core Foundation APIs.
unsafe impl Send for MenuWake {}
unsafe impl Sync for MenuWake {}
impl MenuWake {
    fn new() -> Result<Self> {
        extern "C" fn perform(_: *const std::ffi::c_void) {
            let pointer = DELEGATE.load(Ordering::SeqCst);
            if !pointer.is_null() {
                unsafe { &*pointer }.drain();
            }
        }
        let mut context = CFRunLoopSourceContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform,
        };
        let source = unsafe { CFRunLoopSourceCreate(std::ptr::null(), 0, &mut context) };
        ensure!(!source.is_null(), "无法创建界面事件源");
        let source = unsafe { CFRunLoopSource::wrap_under_create_rule(source) };
        unsafe {
            CFRunLoopAddSource(
                CFRunLoopGetMain(),
                source.as_concrete_TypeRef(),
                kCFRunLoopCommonModes,
            );
        }
        Ok(Self(source))
    }
    fn signal(&self) {
        unsafe {
            CFRunLoopSourceSignal(self.0.as_concrete_TypeRef());
            CFRunLoopWakeUp(CFRunLoopGetMain());
        }
    }
}

pub fn run() -> Result<()> {
    objc2::rc::autoreleasepool(|_| {
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
        // Common modes include menu tracking, so results arrive while the menu stays open.
        let source = Arc::new(MenuWake::new()?);
        let app = App::new({
            let source = source.clone();
            move || source.signal()
        });
        let this = Delegate::alloc(mtm).set_ivars(Ivars {
            app,
            mac: RefCell::new(Mac::default()),
        });
        let delegate: Retained<Delegate> = unsafe { msg_send![super(this), init] };
        DELEGATE.store(Retained::as_ptr(&delegate).cast_mut(), Ordering::SeqCst);
        let application = NSApplication::sharedApplication(mtm);
        application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        application.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        application.run();
        unsafe {
            CFRunLoopSourceInvalidate(source.0.as_concrete_TypeRef());
        }
        DELEGATE.store(std::ptr::null_mut(), Ordering::SeqCst);
        Ok(())
    })
}
