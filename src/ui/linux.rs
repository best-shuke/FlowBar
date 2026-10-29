use super::{Action, App, Entry, Form, Login, Phase, Shell, View, icon};
use crate::{engine::Event, update};
use anyhow::{Context, Result, ensure};
use gtk::{gdk, gio, glib, pango, prelude::*};
use std::{
    cell::{Cell, RefCell},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::OnceLock,
    time::Duration,
};

pub const LOGIN_HINT: &str =
    "登录自启与自动连接分别设置；使用 TUN 时，登录后自动连接会请求管理员授权。";
const APP_ID: &str = "dev.local.FlowBar";
const CSS: &str = "
.flowbar-caption { font-weight: bold; font-size: smaller; opacity: 0.6; }
.flowbar-card {
    background-color: alpha(currentColor, 0.04);
    border: 1px solid alpha(currentColor, 0.12);
    border-radius: 10px;
    padding: 12px 14px;
}
.flowbar-secondary { font-size: smaller; opacity: 0.7; }
.flowbar-port { font-feature-settings: \"tnum\"; }
.flowbar-dot { font-size: 9px; }
.flowbar-dot.stopped { opacity: 0.3; }
.flowbar-dot.running { color: #2ec27e; }
.flowbar-dot.busy { color: #e5a50a; }
.flowbar-dot.failed, .flowbar-failed { color: #e01b24; }
";

thread_local! {
    static SHELL: RefCell<Option<Rc<Linux>>> = const { RefCell::new(None) };
}
/// Calls into the front end without holding the thread-local borrow, so callbacks may nest.
fn with_shell(f: impl FnOnce(&Linux)) {
    let shell = SHELL.with(|cell| cell.borrow().clone());
    if let Some(shell) = shell {
        f(&shell);
    }
}
/// Runs an action from an idle callback: the tray calls in from its own thread, and widget
/// signals should not open modal dialogs while they are being emitted.
fn post(action: Action) {
    glib::idle_add_once(move || with_shell(|shell| shell.app.perform(shell, action)));
}
fn commit_later() {
    glib::idle_add_once(|| with_shell(|shell| shell.app.commit(shell)));
}

struct Linux {
    app: App,
    application: gtk::Application,
    /// Missing when the desktop has no StatusNotifierItem host; closing the window then quits.
    tray: RefCell<Option<ksni::blocking::Handle<Tray>>>,
    /// The model last sent to the tray; each update resends the whole menu over D-Bus.
    shown: RefCell<Model>,
    window: RefCell<Option<Window>>,
    /// Set while render() updates widgets, whose change signals must then not act.
    rendering: Cell<bool>,
}
#[derive(Clone)]
struct Window {
    native: gtk::ApplicationWindow,
    profiles: gtk::DropDown,
    stats: gtk::Label,
    import: gtk::Button,
    reveal: gtk::Button,
    tun: gtk::CheckButton,
    tun_hint: gtk::Label,
    ports: Vec<gtk::Entry>,
    login: gtk::CheckButton,
    auto: gtk::CheckButton,
    hint: gtk::Label,
    auto_update: gtk::CheckButton,
    update: gtk::Button,
    dot: gtk::Label,
    status: gtk::Label,
    logs: gtk::Button,
    start: gtk::Button,
}

impl Shell for Linux {
    fn verify_smoke_render(&self) {
        let view = self.app.view();
        if let Some(window) = self.window.borrow().as_ref() {
            let selected = window
                .profiles
                .selected_item()
                .and_downcast::<gtk::StringObject>()
                .map(|s| s.string().to_string());
            assert_eq!(
                selected.as_ref(),
                view.profile_index.map(|i| &view.profiles[i])
            );
            assert_eq!(window.stats.text(), view.stats);
        }
    }
    fn render(&self) {
        let view = self.app.view();
        self.update_tray(&view);
        // Widgets are reference counted; a clone lets their signals reach the state.
        let Some(window) = self.window.borrow().clone() else {
            return;
        };
        let reset_ports = self.app.take_reset_ports();
        self.rendering.set(true);
        let shown = window
            .profiles
            .model()
            .and_downcast::<gtk::StringList>()
            .map(|list| {
                (0..list.n_items())
                    .filter_map(|index| list.string(index))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if shown != view.profiles {
            let names: Vec<_> = view.profiles.iter().map(String::as_str).collect();
            window
                .profiles
                .set_model(Some(&gtk::StringList::new(&names)));
        }
        window.profiles.set_selected(
            view.profile_index
                .map_or(gtk::INVALID_LIST_POSITION, |index| index as u32),
        );
        window.profiles.set_sensitive(view.switchable);
        window.stats.set_text(&view.stats);
        window.import.set_sensitive(view.editable);
        window.reveal.set_sensitive(view.imported);
        window.tun.set_active(view.tun);
        window.tun.set_sensitive(view.editable);
        window.tun_hint.set_text(view.tun_hint);
        let focus = gtk::prelude::GtkWindowExt::focus(&window.native);
        for (entry, port) in window.ports.iter().zip(view.ports) {
            let editing = focus
                .as_ref()
                .is_some_and(|widget| widget.is_ancestor(entry));
            if reset_ports || !editing {
                entry.set_text(&port.to_string());
            }
            entry.set_sensitive(view.editable);
            entry.set_tooltip_text(view.ports_hint);
        }
        window.login.set_active(view.login == Login::Enabled);
        window.auto.set_active(view.auto_connect);
        window.hint.set_text(view.login_hint);
        window.auto_update.set_active(view.auto_update);
        window.update.set_label(&view.update_title);
        window.update.set_sensitive(view.update_enabled);
        let state = match view.phase {
            Phase::Running => "running",
            Phase::Starting | Phase::Stopping => "busy",
            Phase::Failed => "failed",
            Phase::Stopped => "stopped",
        };
        window.dot.set_css_classes(&["flowbar-dot", state]);
        window.status.set_text(&view.message);
        window.status.set_tooltip_text(Some(&view.message));
        if view.phase == Phase::Failed {
            window.status.add_css_class("flowbar-failed");
        } else {
            window.status.remove_css_class("flowbar-failed");
        }
        window.logs.set_sensitive(view.logs);
        window.start.set_label(view.start_title);
        window.start.set_sensitive(view.start_enabled);
        self.rendering.set(false);
    }
    fn show_settings(&self) {
        self.app.load_summary();
        if self.window.borrow().is_none() {
            let window = self.make_window();
            *self.window.borrow_mut() = Some(window);
        }
        self.render();
        if let Some(window) = self.window.borrow().clone() {
            window.native.present();
        }
    }
    fn close_settings(&self) {
        // Destroying skips close-request, which would quit without a tray.
        let window = self.window.borrow().clone();
        if let Some(window) = window {
            self.app.commit(self);
            self.forget_window(&window.native);
            window.native.destroy();
        }
    }
    fn settings_open(&self) -> bool {
        self.window.borrow().is_some()
    }
    fn form(&self) -> Option<Form> {
        let window = self.window.borrow().clone()?;
        Some(Form {
            tun: window.tun.is_active(),
            auto_connect: window.auto.is_active(),
            auto_update: window.auto_update.is_active(),
            ports: window.ports.iter().map(|e| e.text().to_string()).collect(),
        })
    }
    #[allow(deprecated)]
    fn choose_profile(&self) -> Option<PathBuf> {
        let parent = self.window.borrow().clone().map(|w| w.native);
        let chooser = gtk::FileChooserNative::new(
            Some("导入 YAML"),
            parent.as_ref(),
            gtk::FileChooserAction::Open,
            Some("导入"),
            Some("取消"),
        );
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("mihomo YAML"));
        for pattern in ["*.yaml", "*.yml", "*.YAML", "*.YML"] {
            filter.add_pattern(pattern);
        }
        chooser.add_filter(&filter);
        chooser.set_modal(true);
        let chosen = Rc::new(RefCell::new(None));
        let main_loop = glib::MainLoop::new(None, false);
        chooser.connect_response({
            let chosen = chosen.clone();
            let main_loop = main_loop.clone();
            move |chooser, response| {
                if response == gtk::ResponseType::Accept {
                    *chosen.borrow_mut() = chooser.file().and_then(|file| file.path());
                }
                main_loop.quit();
            }
        });
        chooser.show();
        main_loop.run();
        chosen.take()
    }
    #[allow(deprecated)]
    fn alert(&self, title: &str, message: &str, buttons: &[&str]) -> usize {
        let dialog = gtk::MessageDialog::builder()
            .modal(true)
            .message_type(gtk::MessageType::Info)
            .text(title)
            .secondary_text(message)
            .build();
        if let Some(window) = self.window.borrow().clone() {
            dialog.set_transient_for(Some(&window.native));
        }
        let labels = if buttons.is_empty() {
            &["好"]
        } else {
            buttons
        };
        for (index, label) in labels.iter().enumerate() {
            dialog.add_button(label, gtk::ResponseType::Other(index as u16));
        }
        dialog.set_default_response(gtk::ResponseType::Other(0));
        let choice = Rc::new(Cell::new(buttons.len()));
        let main_loop = glib::MainLoop::new(None, false);
        dialog.connect_response({
            let choice = choice.clone();
            let main_loop = main_loop.clone();
            move |dialog, response| {
                if let gtk::ResponseType::Other(index) = response {
                    choice.set(index as usize);
                }
                dialog.destroy();
                main_loop.quit();
            }
        });
        dialog.present();
        main_loop.run();
        choice.get()
    }
    /// The tray thread holds its own copy of the menu, so it needs the new groups too.
    fn groups_changed(&self) {
        self.update_tray(&self.app.view());
    }
    fn schedule_updates(&self) {
        let emit = self.app.emit.clone();
        glib::timeout_add_seconds_local_once(10, move || {
            emit(Event::Update(update::Event::Due));
            glib::timeout_add_seconds_local(3600, move || {
                emit(Event::Update(update::Event::Due));
                glib::ControlFlow::Continue
            });
        });
    }
    /// Renders the settings window to a PNG once it has been laid out.
    fn snapshot(&self, path: &Path) {
        let Some(window) = self.window.borrow().clone() else {
            return;
        };
        let path = path.to_path_buf();
        glib::timeout_add_local_once(Duration::from_millis(800), move || {
            let widget = &window.native;
            let (width, height) = (widget.width() as f64, widget.height() as f64);
            let paintable = gtk::WidgetPaintable::new(Some(widget));
            let snapshot = gtk::Snapshot::new();
            paintable.snapshot(&snapshot, width, height);
            let renderer = window.native.renderer();
            if let (Some(node), Some(renderer)) = (snapshot.to_node(), renderer) {
                let texture = renderer.render_texture(&node, None);
                if let Err(error) = texture.save_to_png(&path) {
                    eprintln!("无法保存截图：{error}");
                }
            }
        });
    }
    fn quit(&self) {
        self.app.stop();
        self.application.quit();
    }
}

impl Linux {
    fn new(application: gtk::Application) -> Self {
        Self {
            app: App::new(|| {
                glib::idle_add_once(|| with_shell(|shell| shell.app.drain(shell)));
            }),
            application,
            tray: RefCell::new(None),
            shown: RefCell::new(Model::default()),
            window: RefCell::new(None),
            rendering: Cell::new(false),
        }
    }
    fn setup(&self) {
        let provider = gtk::CssProvider::new();
        provider.load_from_data(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        if !std::env::args().any(|a| a == "--ui-smoke") {
            use ksni::blocking::TrayMethods;
            match (Tray {
                model: Model::default(),
            })
            .spawn()
            {
                Ok(handle) => *self.tray.borrow_mut() = Some(handle),
                Err(error) => {
                    eprintln!("FlowBar: 系统托盘不可用（{error}）；关闭设置窗口会退出 FlowBar。")
                }
            }
        }
        self.app.launch(self);
        // Without a tray icon the window is the only way in.
        if self.tray.borrow().is_none() && !self.settings_open() {
            self.show_settings();
        }
    }
    /// Drops the state of a closing window. GTK destroys a window only once the last
    /// reference is gone, so this cannot wait for the destroy signal.
    fn forget_window(&self, native: &gtk::ApplicationWindow) {
        let mut window = self.window.borrow_mut();
        if window.as_ref().is_some_and(|w| &w.native == native) {
            *window = None;
        }
    }
    fn update_tray(&self, view: &View) {
        let Some(handle) = self.tray.borrow().clone() else {
            return;
        };
        let model = Model {
            tooltip: format!("FlowBar · {}", view.message),
            running: view.phase == Phase::Running,
            entries: self.app.menu(view.login),
        };
        // Updating blocks on the tray thread, so only changes are sent.
        if *self.shown.borrow() == model {
            return;
        }
        *self.shown.borrow_mut() = model.clone();
        handle.update(move |tray| tray.model = model);
    }
    fn make_window(&self) -> Window {
        let native = gtk::ApplicationWindow::builder()
            .application(&self.application)
            .title("FlowBar")
            .resizable(false)
            .build();
        native.connect_is_active_notify(|window| {
            if window.is_active() {
                with_shell(|shell| {
                    if !shell.rendering.get() {
                        shell.app.load_summary();
                        shell.render();
                    }
                });
            }
        });
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_margin_top(14);
        content.set_margin_bottom(16);
        content.set_margin_start(20);
        content.set_margin_end(20);
        content.set_size_request(420, -1);
        let section = |title: &str, accessory: Option<&gtk::Label>| {
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            header.set_margin_start(14);
            header.set_margin_end(14);
            header.set_margin_bottom(6);
            if content.first_child().is_some() {
                header.set_margin_top(18);
            }
            let caption = label(title, "flowbar-caption");
            caption.set_hexpand(true);
            header.append(&caption);
            if let Some(accessory) = accessory {
                header.append(accessory);
            }
            content.append(&header);
            let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
            card.add_css_class("flowbar-card");
            content.append(&card);
            card
        };

        let card = section("配置", None);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let profiles = profile_list();
        let import = button("导入 YAML…", Action::Import);
        let reveal = button("打开所在文件夹", Action::Reveal);
        row.append(&profiles);
        row.append(&import);
        row.append(&reveal);
        card.append(&row);
        let stats = label("", "flowbar-secondary");
        stats.set_ellipsize(pango::EllipsizeMode::End);
        card.append(&stats);

        let card = section("连接", None);
        let tun = check("使用 TUN 接管流量", false);
        card.append(&tun);
        let tun_hint = hint();
        card.append(&tun_hint);
        let grid = gtk::Grid::new();
        grid.set_column_spacing(12);
        grid.set_row_spacing(4);
        grid.set_column_homogeneous(true);
        grid.set_margin_top(4);
        let mut ports = vec![];
        for (column, title) in ["HTTP/SOCKS", "控制接口", "DNS"].into_iter().enumerate() {
            grid.attach(&label(title, "flowbar-secondary"), column as i32, 0, 1, 1);
            let entry = gtk::Entry::new();
            entry.set_max_length(5);
            entry.set_width_chars(6);
            entry.set_input_purpose(gtk::InputPurpose::Digits);
            entry.add_css_class("flowbar-port");
            entry.connect_activate(|_| commit_later());
            // Moving focus away saves too, not only Return.
            let focus = gtk::EventControllerFocus::new();
            focus.connect_leave(|_| commit_later());
            entry.add_controller(focus);
            grid.attach(&entry, column as i32, 1, 1, 1);
            ports.push(entry);
        }
        card.append(&grid);

        let version = label(&format!("版本 {}", update::CURRENT), "flowbar-secondary");
        let card = section("启动与更新", Some(&version));
        let login = check("登录时打开 FlowBar", true);
        let auto = check("打开 FlowBar 后自动连接", false);
        let login_hint = hint();
        card.append(&login);
        card.append(&auto);
        card.append(&login_hint);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let auto_update = check("自动检查更新", false);
        auto_update.set_hexpand(true);
        let update = gtk::Button::with_label("检查更新…");
        update.connect_clicked(|_| {
            with_shell(|shell| {
                if let Some(action) = shell.app.update_action() {
                    post(action);
                }
            })
        });
        row.append(&auto_update);
        row.append(&update);
        card.append(&row);

        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        footer.set_margin_top(18);
        let dot = label("●", "flowbar-dot");
        dot.set_margin_start(2);
        // The text stays selectable so errors can be copied.
        let status = label("", "flowbar-status");
        status.set_hexpand(true);
        status.set_wrap(true);
        status.set_wrap_mode(pango::WrapMode::WordChar);
        status.set_lines(2);
        status.set_ellipsize(pango::EllipsizeMode::End);
        status.set_max_width_chars(18);
        status.set_selectable(true);
        status.set_focusable(false);
        let logs = button("查看日志", Action::Logs);
        let start = button("启动连接", Action::Connect);
        start.add_css_class("suggested-action");
        for widget in [
            dot.upcast_ref::<gtk::Widget>(),
            status.upcast_ref(),
            logs.upcast_ref(),
            start.upcast_ref(),
        ] {
            widget.set_valign(gtk::Align::Center);
            footer.append(widget);
        }
        content.append(&footer);
        native.set_child(Some(&content));
        native.set_default_widget(Some(&start));

        native.connect_close_request(|native| {
            with_shell(|shell| {
                shell.app.commit(shell);
                shell.forget_window(native);
                if shell.tray.borrow().is_none() {
                    shell.quit();
                }
            });
            glib::Propagation::Proceed
        });
        Window {
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
            hint: login_hint,
            auto_update,
            update,
            dot,
            status,
            logs,
            start,
        }
    }
}

fn label(text: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class(class);
    label
}
fn hint() -> gtk::Label {
    let hint = label("", "flowbar-secondary");
    hint.set_wrap(true);
    hint.set_wrap_mode(pango::WrapMode::WordChar);
    hint.set_max_width_chars(40);
    hint.set_margin_start(28);
    hint
}
/// The profile choice. Its button shortens long names so they cannot widen the window;
/// the list shows them in full.
fn profile_list() -> gtk::DropDown {
    let factory = |ellipsize: bool| {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(move |_, item| {
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            if ellipsize {
                label.set_ellipsize(pango::EllipsizeMode::End);
                label.set_max_width_chars(16);
            }
            if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                item.set_child(Some(&label));
            }
        });
        factory.connect_bind(|_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            if let (Some(label), Some(name)) = (
                item.child().and_downcast::<gtk::Label>(),
                item.item().and_downcast::<gtk::StringObject>(),
            ) {
                label.set_text(&name.string());
            }
        });
        factory
    };
    let list = gtk::DropDown::new(None::<gtk::StringList>, None::<gtk::Expression>);
    list.set_factory(Some(&factory(true)));
    list.set_list_factory(Some(&factory(false)));
    list.set_hexpand(true);
    list.set_valign(gtk::Align::Center);
    list.connect_selected_notify(|list| {
        let name = list
            .selected_item()
            .and_downcast::<gtk::StringObject>()
            .map(|name| name.string().to_string());
        with_shell(|shell| {
            if let Some(name) = name.filter(|_| !shell.rendering.get()) {
                post(Action::Profile(name));
            }
        })
    });
    list
}
fn button(title: &str, action: Action) -> gtk::Button {
    let button = gtk::Button::with_label(title);
    button.connect_clicked(move |_| post(action.clone()));
    button
}
/// A check box that saves the settings window, or for the login item toggles it.
fn check(title: &str, login: bool) -> gtk::CheckButton {
    let check = gtk::CheckButton::with_label(title);
    check.connect_toggled(move |_| {
        with_shell(|shell| {
            if shell.rendering.get() {
                return;
            }
            if login {
                post(Action::Login);
            } else {
                commit_later();
            }
        })
    });
    check
}

#[derive(Clone, Default, PartialEq)]
struct Model {
    tooltip: String,
    running: bool,
    entries: Vec<Entry>,
}
/// The StatusNotifierItem, served from its own thread; it only reads the model and posts actions.
struct Tray {
    model: Model,
}
impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "flowbar".into()
    }
    fn title(&self) -> String {
        "FlowBar".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        static ICONS: OnceLock<[Vec<ksni::Icon>; 2]> = OnceLock::new();
        let icons = ICONS.get_or_init(|| {
            [false, true].map(|running| {
                [22, 32, 48]
                    .into_iter()
                    .map(|size| ksni::Icon {
                        width: size as i32,
                        height: size as i32,
                        data: icon::pixels(size, running)
                            .iter()
                            .flat_map(|pixel| pixel.to_be_bytes())
                            .collect(),
                    })
                    .collect()
            })
        });
        icons[usize::from(self.model.running)].clone()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "FlowBar".into(),
            description: self.model.tooltip.clone(),
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        post(Action::Settings);
    }
    fn menu_about_to_show(&mut self) {
        glib::idle_add_once(|| with_shell(|shell| shell.app.refresh_groups()));
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        menu_items(&self.model.entries)
    }
}
fn menu_items(entries: &[Entry]) -> Vec<ksni::MenuItem<Tray>> {
    use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
    entries
        .iter()
        .map(|entry| match entry {
            Entry::Separator => ksni::MenuItem::Separator,
            Entry::Item {
                title,
                detail,
                action,
                enabled,
                checked,
            } => {
                // D-Bus menu hosts disagree about tab stops. Keep the complete result in
                // the label so a panel cannot interpret it as a clipped accelerator.
                let label = match detail {
                    Some(detail) => format!("{}    {detail}", menu_label(title)),
                    None => menu_label(title),
                };
                let action = action.clone();
                let activate = Box::new(move |_: &mut Tray| {
                    if let Some(action) = &action {
                        post(action.clone());
                    }
                });
                match checked {
                    Some(checked) => CheckmarkItem {
                        label,
                        enabled: *enabled,
                        checked: *checked,
                        activate,
                        ..Default::default()
                    }
                    .into(),
                    None => StandardItem {
                        label,
                        enabled: *enabled,
                        activate,
                        ..Default::default()
                    }
                    .into(),
                }
            }
            Entry::Menu { title, entries, .. } => SubMenu {
                label: menu_label(title),
                submenu: menu_items(entries),
                ..Default::default()
            }
            .into(),
        })
        .collect()
}
/// DBusMenu labels use underscores for access keys; node names keep theirs.
fn menu_label(title: &str) -> String {
    title.replace('_', "__")
}

pub fn open(path: &Path, reveal: bool) {
    if reveal && show_item(path).is_ok() {
        return;
    }
    let target = if reveal {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    open_url(&gio::File::for_path(target).uri());
}
/// Selects the file in the desktop's file manager.
fn show_item(path: &Path) -> Result<()> {
    let bus = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)?;
    let uri = gio::File::for_path(path).uri().to_string();
    bus.call_sync(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1",
        "ShowItems",
        Some(&(vec![uri], "").to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        2000,
        None::<&gio::Cancellable>,
    )?;
    Ok(())
}
pub fn open_url(url: &str) {
    if let Err(error) = gio::AppInfo::launch_default_for_uri(url, None::<&gio::AppLaunchContext>) {
        eprintln!("无法打开 {url}：{error}");
    }
}

/// An XDG autostart entry that starts this executable.
fn autostart() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    config.join("autostart").join(format!("{APP_ID}.desktop"))
}
/// The Exec key for this executable. The value is unescaped once as a string and once as
/// a quoted argument, so reserved characters carry two levels of escaping.
fn exec_line() -> Result<String> {
    let path = std::env::current_exe()?
        .into_os_string()
        .into_string()
        .ok()
        .context("应用路径编码无效")?;
    let mut quoted = String::from("Exec=\"");
    for c in path.chars() {
        match c {
            '"' | '`' | '$' => {
                quoted.push_str(r"\\");
                quoted.push(c);
            }
            '\\' => quoted.push_str(r"\\\\"),
            '%' => quoted.push_str("%%"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    Ok(quoted)
}
pub fn login_status() -> Login {
    let enabled = exec_line().is_ok_and(|exec| {
        fs::read_to_string(autostart()).is_ok_and(|entry| {
            entry.lines().any(|line| line == exec) && !entry.contains("Hidden=true")
        })
    });
    if enabled {
        Login::Enabled
    } else {
        Login::Disabled
    }
}
/// Whether a FlowBar autostart entry exists, also one that starts another copy.
pub fn login_entry() -> bool {
    fs::symlink_metadata(autostart()).is_ok()
}
pub fn remove_login() -> Result<()> {
    fs::remove_file(autostart()).context("无法移除自启动项")
}
pub fn toggle_login() -> Result<()> {
    let path = autostart();
    if login_status() == Login::Enabled {
        return remove_login();
    }
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=FlowBar\nComment=rill 代理托盘\n{}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        exec_line()?
    );
    fs::create_dir_all(path.parent().unwrap())?;
    crate::config::atomic_write(&path, entry.as_bytes(), 0o644).context("无法写入自启动项")
}

pub fn run() -> Result<()> {
    // The UI test runs next to an installed FlowBar without forwarding to it.
    let flags = if std::env::args().any(|a| a == "--ui-smoke") {
        gio::ApplicationFlags::NON_UNIQUE
    } else {
        gio::ApplicationFlags::empty()
    };
    let application = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(flags)
        .build();
    // A second launch activates the first instance, which shows its settings window.
    application.connect_activate(|application| {
        let existing = SHELL.with(|cell| cell.borrow().clone());
        if let Some(shell) = existing {
            shell.show_settings();
            return;
        }
        let shell = Rc::new(Linux::new(application.clone()));
        SHELL.with(|cell| *cell.borrow_mut() = Some(shell.clone()));
        shell.setup();
    });
    let hold = application.hold();
    // FlowBar's own arguments are not GTK options.
    let code = application.run_with_args::<&str>(&[]);
    drop(hold);
    if let Some(shell) = SHELL.with(|cell| cell.borrow_mut().take()) {
        shell.app.stop();
        if let Some(tray) = shell.tray.borrow_mut().take() {
            tray.shutdown().wait();
        }
    }
    ensure!(code == glib::ExitCode::SUCCESS, "GTK 界面异常退出");
    Ok(())
}
