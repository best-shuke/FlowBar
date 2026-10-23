use super::{Action, App, Entry, Form, Login, Phase, Shell, View, icon};
use crate::{engine::Event, update};
use anyhow::{Result, ensure};
use std::{
    cell::RefCell,
    ffi::{OsStr, c_void},
    os::windows::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
    ptr::{null, null_mut},
    rc::Rc,
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};
use windows_sys::Win32::{
    Foundation::{
        COLORREF, ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT,
        WPARAM,
    },
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, CreateBitmap, CreateDIBSection,
        CreateFontIndirectW, CreatePen, CreateSolidBrush, DIB_RGB_COLORS, DT_CALCRECT, DT_NOPREFIX,
        DT_WORDBREAK, DeleteObject, DrawTextW, Ellipse, EndPaint, FillRect, GetDC, GetMonitorInfoW,
        GetStockObject, HBRUSH, HDC, HFONT, InvalidateRect, LOGFONTW, MONITOR_DEFAULTTONEAREST,
        MONITORINFO, MonitorFromWindow, NULL_PEN, PAINTSTRUCT, PS_SOLID, ReleaseDC, RoundRect,
        SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
    },
    System::{
        Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx},
        LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW},
        Registry::{
            HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
            RegSetKeyValueW,
        },
        Threading::CreateMutexW,
    },
    UI::{
        Controls::{
            BST_CHECKED,
            Dialogs::{
                GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY,
                OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
            },
            EM_SETLIMITTEXT, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx,
            TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TDCBF_OK_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION,
            TDN_CREATED,
        },
        HiDpi::{
            AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem,
            GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext,
            SystemParametersInfoForDpi,
        },
        Input::KeyboardAndMouse::{EnableWindow, GetFocus, SetFocus},
        Shell::{
            NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
            NIM_SETVERSION, NIN_SELECT, NOTIFY_ICON_MESSAGE, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
            Shell_NotifyIconW, ShellExecuteW,
        },
        WindowsAndMessaging::{
            AllowSetForegroundWindow, AppendMenuW, BM_GETCHECK, BM_SETCHECK, BN_CLICKED,
            BS_AUTOCHECKBOX, BS_PUSHBUTTON, CW_USEDEFAULT, CreateIconIndirect, CreatePopupMenu,
            CreateWindowExW, DefWindowProcW, DeleteMenu, DestroyIcon, DestroyMenu, DestroyWindow,
            DispatchMessageW, EN_KILLFOCUS, ES_AUTOHSCROLL, ES_NUMBER, FindWindowW, GetDlgCtrlID,
            GetDlgItem, GetMenuItemCount, GetMessageW, GetSystemMetrics, GetWindowTextLengthW,
            GetWindowTextW, GetWindowThreadProcessId, HICON, HMENU, ICONINFO, IDC_ARROW, IDCANCEL,
            IDOK, IMAGE_ICON, IsDialogMessageW, IsIconic, KillTimer, LR_DEFAULTCOLOR, LoadCursorW,
            LoadImageW, MB_OK, MB_OKCANCEL, MB_SETFOREGROUND, MF_BYPOSITION, MF_CHECKED, MF_GRAYED,
            MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, MessageBoxW, MoveWindow, NONCLIENTMETRICSW,
            PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SM_CXICON,
            SM_CXSMICON, SPI_GETNONCLIENTMETRICS, SW_RESTORE, SW_SHOW, SW_SHOWNORMAL,
            SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetForegroundWindow, SetTimer,
            SetWindowPos, SetWindowTextW, ShowWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON,
            TrackPopupMenuEx, TranslateMessage, WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU,
            WM_CTLCOLORBTN, WM_CTLCOLORSTATIC, WM_DPICHANGED, WM_ENDSESSION, WM_ERASEBKGND,
            WM_INITMENUPOPUP, WM_NCDESTROY, WM_NULL, WM_PAINT, WM_PRINTCLIENT, WM_SETFONT,
            WM_TIMER, WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_EX_CLIENTEDGE,
            WS_MINIMIZEBOX, WS_OVERLAPPED, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
        },
    },
};

pub const LOGIN_HINT: &str =
    "登录自启与自动连接分别设置；使用 TUN 时，登录后自动连接会请求管理员授权。";
const HOST_CLASS: &str = "FlowBar.Host";
const SETTINGS_CLASS: &str = "FlowBar.Settings";
/// Posted by background threads; the tray icon reports clicks with WM_TRAY.
const WM_WAKE: u32 = WM_APP + 1;
const WM_TRAY: u32 = WM_APP + 2;
const WM_SHOW: u32 = WM_APP + 3;
const WM_COMMIT: u32 = WM_APP + 4;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
// Static control styles, from WinUser.h.
const SS_RIGHT: u32 = 0x2;
const SS_NOPREFIX: u32 = 0x80;
const SS_ENDELLIPSIS: u32 = 0x4000;
const FIRST_CHECK: usize = 1;
const HOURLY_CHECK: usize = 2;
/// Custom task dialog buttons are numbered from here.
const BUTTON_BASE: i32 = 100;
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

static HOST: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
thread_local! {
    static SHELL: RefCell<Option<Rc<Win>>> = const { RefCell::new(None) };
}
/// Calls into the front end without holding the thread-local borrow, so window messages may nest.
fn with_shell<R>(f: impl FnOnce(&Win) -> R) -> Option<R> {
    let shell = SHELL.with(|cell| cell.borrow().clone());
    shell.map(|shell| f(&shell))
}

// The settings window, laid out in 96-DPI pixels like the macOS window it mirrors.
const CLIENT: (i32, i32) = (460, 510);
const CARDS: [[i32; 4]; 3] = [[20, 36, 420, 72], [20, 146, 420, 126], [20, 310, 420, 132]];
const FOOTER_MIDLINE: i32 = 480;
const STATUS_RECT: [i32; 4] = [34, 472, 188, 16];
const PROFILE: i32 = 101;
const IMPORT: i32 = 102;
const REVEAL: i32 = 103;
const STATS: i32 = 104;
const TUN: i32 = 105;
const TUN_HINT: i32 = 106;
const PORTS: [i32; 3] = [107, 108, 109];
const LOGIN: i32 = 110;
const AUTO: i32 = 111;
const HINT: i32 = 112;
const AUTO_UPDATE: i32 = 113;
const UPDATE: i32 = 114;
const STATUS: i32 = 115;
const LOGS: i32 = 116;
const START: i32 = 117;
const VERSION: i32 = 118;
/// Fixed labels: section captions and port titles.
const LABELS: i32 = 130;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bold,
    Caption,
    Secondary,
    Wrapped,
    Status,
    Button,
    Check,
    Port,
}
struct Control {
    id: i32,
    kind: Kind,
    text: &'static str,
    rect: [i32; 4],
    /// Drawn on a section card rather than the window background.
    card: bool,
}
const fn control(id: i32, kind: Kind, text: &'static str, rect: [i32; 4], card: bool) -> Control {
    Control {
        id,
        kind,
        text,
        rect,
        card,
    }
}
const CONTROLS: &[Control] = &[
    control(LABELS, Kind::Caption, "配置", [34, 16, 200, 16], false),
    control(PROFILE, Kind::Bold, "", [34, 50, 148, 20], true),
    control(IMPORT, Kind::Button, "导入 YAML…", [186, 45, 106, 28], true),
    control(
        REVEAL,
        Kind::Button,
        "在资源管理器中显示",
        [296, 45, 136, 28],
        true,
    ),
    control(STATS, Kind::Secondary, "", [34, 82, 392, 16], true),
    control(LABELS + 1, Kind::Caption, "连接", [34, 126, 200, 16], false),
    control(
        TUN,
        Kind::Check,
        "使用 TUN 接管流量",
        [34, 158, 392, 20],
        true,
    ),
    control(TUN_HINT, Kind::Wrapped, "", [54, 180, 372, 30], true),
    control(
        LABELS + 2,
        Kind::Secondary,
        "HTTP/SOCKS",
        [34, 214, 122, 16],
        true,
    ),
    control(
        LABELS + 3,
        Kind::Secondary,
        "控制接口",
        [169, 214, 122, 16],
        true,
    ),
    control(
        LABELS + 4,
        Kind::Secondary,
        "DNS",
        [304, 214, 122, 16],
        true,
    ),
    control(PORTS[0], Kind::Port, "", [34, 232, 122, 24], true),
    control(PORTS[1], Kind::Port, "", [169, 232, 122, 24], true),
    control(PORTS[2], Kind::Port, "", [304, 232, 122, 24], true),
    control(
        LABELS + 5,
        Kind::Caption,
        "启动与更新",
        [34, 290, 200, 16],
        false,
    ),
    control(VERSION, Kind::Secondary, "", [226, 290, 200, 16], false),
    control(
        LOGIN,
        Kind::Check,
        "登录时打开 FlowBar",
        [34, 322, 392, 20],
        true,
    ),
    control(
        AUTO,
        Kind::Check,
        "打开 FlowBar 后自动连接",
        [34, 348, 392, 20],
        true,
    ),
    control(HINT, Kind::Wrapped, "", [54, 370, 372, 30], true),
    control(
        AUTO_UPDATE,
        Kind::Check,
        "自动检查更新",
        [34, 406, 240, 20],
        true,
    ),
    control(UPDATE, Kind::Button, "检查更新…", [292, 402, 140, 28], true),
    control(STATUS, Kind::Status, "", STATUS_RECT, false),
    control(LOGS, Kind::Button, "查看日志", [228, 466, 96, 28], false),
    control(START, Kind::Button, "启动连接", [330, 466, 102, 28], false),
];

struct Win {
    app: App,
    host: HWND,
    instance: HINSTANCE,
    /// Stopped and running tray icons.
    icons: [HICON; 2],
    window: RefCell<Option<Window>>,
    popup: RefCell<Option<Popup>>,
}
struct Window {
    hwnd: HWND,
    dpi: u32,
    fonts: Fonts,
    colors: Colors,
    window_brush: HBRUSH,
    card_brush: HBRUSH,
    phase: Phase,
    /// Top of the status dot, in physical pixels; it follows the status text's first line.
    dot_top: i32,
}
#[derive(Clone, Copy)]
struct Fonts {
    normal: HFONT,
    bold: HFONT,
    small: HFONT,
    caption: HFONT,
}
#[derive(Clone, Copy)]
struct Colors {
    text: COLORREF,
    grey: COLORREF,
    window: COLORREF,
    card: COLORREF,
    border: COLORREF,
}
/// The open status menu: the action for each item ID, and the groups submenu, which is
/// rebuilt from the latest groups each time it opens.
struct Popup {
    actions: Vec<Action>,
    groups: HMENU,
}

impl Shell for Win {
    fn render(&self) {
        let view = self.app.view();
        self.update_tray(&view);
        let Some(hwnd) = self.settings_hwnd() else {
            return;
        };
        let item = |id| unsafe { GetDlgItem(hwnd, id) };
        set_text(item(PROFILE), &view.profile);
        set_text(item(STATS), &view.stats);
        enable(item(IMPORT), view.editable);
        enable(item(REVEAL), view.imported);
        check(item(TUN), view.tun);
        enable(item(TUN), view.editable);
        set_text(item(TUN_HINT), view.tun_hint);
        let focus = unsafe { GetFocus() };
        for (id, port) in PORTS.into_iter().zip(view.ports) {
            let field = item(id);
            if view.reset_ports || focus != field {
                set_text(field, &port.to_string());
            }
            enable(field, view.editable);
        }
        check(item(LOGIN), view.login == Login::Enabled);
        check(item(AUTO), view.auto_connect);
        set_text(item(HINT), view.login_hint);
        check(item(AUTO_UPDATE), view.auto_update);
        set_text(item(UPDATE), &view.update_title);
        enable(item(UPDATE), view.update_enabled);
        set_text(item(STATUS), &view.message);
        enable(item(LOGS), view.logs);
        set_text(item(START), view.start_title);
        enable(item(START), view.start_enabled);
        if let Some(window) = self.window.borrow_mut().as_mut() {
            window.phase = view.phase;
        }
        self.place_status(hwnd, &view.message);
    }
    fn show_settings(&self) {
        let hwnd = match self.settings_hwnd() {
            Some(hwnd) => hwnd,
            None => {
                let Some(hwnd) = self.make_window() else {
                    return;
                };
                self.app.load_summary();
                hwnd
            }
        };
        self.render();
        unsafe {
            ShowWindow(
                hwnd,
                if IsIconic(hwnd) != 0 {
                    SW_RESTORE
                } else {
                    SW_SHOW
                },
            );
            SetForegroundWindow(hwnd);
        }
    }
    fn close_settings(&self) {
        self.close_window();
    }
    fn settings_open(&self) -> bool {
        self.window.borrow().is_some()
    }
    fn form(&self) -> Option<Form> {
        let hwnd = self.settings_hwnd()?;
        let item = |id| unsafe { GetDlgItem(hwnd, id) };
        let checked =
            |id| unsafe { SendMessageW(item(id), BM_GETCHECK, 0, 0) } == BST_CHECKED as isize;
        Some(Form {
            tun: checked(TUN),
            auto_connect: checked(AUTO),
            auto_update: checked(AUTO_UPDATE),
            ports: PORTS.into_iter().map(|id| text(item(id))).collect(),
        })
    }
    fn choose_profile(&self) -> Option<PathBuf> {
        let filter: Vec<u16> = "mihomo YAML (*.yaml;*.yml)\0*.yaml;*.yml\0所有文件\0*.*\0\0"
            .encode_utf16()
            .collect();
        let title = wide("导入 YAML");
        let mut buffer = vec![0u16; 32768];
        let mut dialog: OPENFILENAMEW = unsafe { std::mem::zeroed() };
        dialog.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        dialog.hwndOwner = self.owner();
        dialog.lpstrFilter = filter.as_ptr();
        dialog.lpstrFile = buffer.as_mut_ptr();
        dialog.nMaxFile = buffer.len() as u32;
        dialog.lpstrTitle = title.as_ptr();
        dialog.Flags = OFN_EXPLORER
            | OFN_FILEMUSTEXIST
            | OFN_PATHMUSTEXIST
            | OFN_HIDEREADONLY
            | OFN_NOCHANGEDIR;
        if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
            return None;
        }
        let length = buffer.iter().position(|&c| c == 0).unwrap_or(0);
        Some(PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
    }
    fn alert(&self, title: &str, message: &str, buttons: &[&str]) -> usize {
        let owner = self.owner();
        let caption = wide("FlowBar");
        // Errors use the app name as their title; only the window caption shows it.
        let instruction = (title != "FlowBar").then(|| wide(title));
        let content = wide(message);
        let labels: Vec<_> = buttons.iter().map(wide).collect();
        let specs: Vec<_> = labels
            .iter()
            .enumerate()
            .map(|(index, label)| TASKDIALOG_BUTTON {
                nButtonID: BUTTON_BASE + index as i32,
                pszButtonText: label.as_ptr(),
            })
            .collect();
        let mut config: TASKDIALOGCONFIG = unsafe { std::mem::zeroed() };
        config.cbSize = size_of::<TASKDIALOGCONFIG>() as u32;
        config.hwndParent = owner;
        config.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION;
        config.dwCommonButtons = if buttons.is_empty() {
            TDCBF_OK_BUTTON
        } else {
            0
        };
        config.pszWindowTitle = caption.as_ptr();
        config.pszMainInstruction = instruction.as_ref().map_or(null(), |i| i.as_ptr());
        config.pszContent = content.as_ptr();
        config.cButtons = specs.len() as u32;
        config.pButtons = specs.as_ptr();
        config.nDefaultButton = if buttons.is_empty() {
            IDOK
        } else {
            BUTTON_BASE
        };
        config.pfCallback = Some(bring_to_front);
        let mut pressed = 0;
        let shown = task_dialog().is_some_and(
            |show| unsafe { show(&config, &mut pressed, null_mut(), null_mut()) } >= 0,
        );
        if !shown {
            // Task dialogs need the common controls 6 manifest.
            let text = wide(format!("{title}\n\n{message}").trim_start_matches("FlowBar\n\n"));
            let style = if buttons.is_empty() {
                MB_OK
            } else {
                MB_OKCANCEL
            };
            pressed = unsafe {
                MessageBoxW(
                    owner,
                    text.as_ptr(),
                    caption.as_ptr(),
                    style | MB_SETFOREGROUND,
                )
            };
            return if pressed == IDOK { 0 } else { buttons.len() };
        }
        match pressed {
            pressed if pressed >= BUTTON_BASE => (pressed - BUTTON_BASE) as usize,
            IDOK if buttons.is_empty() => 0,
            _ => buttons.len(),
        }
    }
    fn schedule_updates(&self) {
        unsafe { SetTimer(self.host, FIRST_CHECK, 10_000, None) };
    }
    fn quit(&self) {
        self.app.stop();
        if let Some(hwnd) = self.settings_hwnd() {
            unsafe { DestroyWindow(hwnd) };
        }
        unsafe { PostQuitMessage(0) };
    }
}

impl Win {
    fn settings_hwnd(&self) -> Option<HWND> {
        self.window.borrow().as_ref().map(|window| window.hwnd)
    }
    /// Dialogs belong to the settings window, or to the hidden host while it is closed.
    fn owner(&self) -> HWND {
        self.settings_hwnd().unwrap_or(self.host)
    }
    fn perform(&self, action: Action) {
        self.app.perform(self, action);
    }

    fn notify_icon(&self, message: NOTIFY_ICON_MESSAGE, running: bool, tip: &str) {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.host;
        data.uID = 1;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = self.icons[usize::from(running)];
        for (slot, unit) in data.szTip[..127].iter_mut().zip(tip.encode_utf16()) {
            *slot = unit;
        }
        unsafe { Shell_NotifyIconW(message, &data) };
        if message == NIM_ADD {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) };
        }
    }
    /// Adds the tray icon, also after Explorer restarts.
    fn add_tray(&self) {
        let view = self.app.view();
        let tip = format!("FlowBar · {}", view.message);
        self.notify_icon(NIM_ADD, view.phase == Phase::Running, &tip);
    }
    fn update_tray(&self, view: &View) {
        let tip = format!("FlowBar · {}", view.message);
        self.notify_icon(NIM_MODIFY, view.phase == Phase::Running, &tip);
    }
    fn remove_tray(&self) {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.host;
        data.uID = 1;
        unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
    }
    /// A left click opens the settings window; a right click or the menu key opens the menu.
    fn tray_event(&self, event: u32, anchor: WPARAM) {
        match event {
            WM_CONTEXTMENU => {
                let x = (anchor & 0xFFFF) as u16 as i16 as i32;
                let y = ((anchor >> 16) & 0xFFFF) as u16 as i16 as i32;
                self.show_menu(x, y);
            }
            NIN_SELECT | NIN_KEYSELECT => self.show_settings(),
            _ => {}
        }
    }
    fn show_menu(&self, x: i32, y: i32) {
        self.app.refresh_groups();
        let mut popup = Popup {
            actions: vec![],
            groups: null_mut(),
        };
        let menu = unsafe { CreatePopupMenu() };
        for entry in self.app.menu() {
            append(menu, &entry, &mut popup);
        }
        *self.popup.borrow_mut() = Some(popup);
        // The menu closes when the user clicks elsewhere only if the host is in front.
        let chosen = unsafe {
            SetForegroundWindow(self.host);
            let chosen = TrackPopupMenuEx(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                x,
                y,
                self.host,
                null(),
            );
            PostMessageW(self.host, WM_NULL, 0, 0);
            DestroyMenu(menu);
            chosen
        };
        // Item IDs start at 1; 0 means the menu was dismissed.
        let popup = self.popup.borrow_mut().take();
        let action = usize::try_from(chosen)
            .ok()
            .and_then(|id| id.checked_sub(1))
            .and_then(|index| popup?.actions.get(index).cloned());
        if let Some(action) = action {
            self.perform(action);
        }
    }
    /// Fills the groups submenu as it opens, with groups fetched after the menu opened.
    fn init_popup(&self, menu: HMENU) {
        let mut popup = self.popup.borrow_mut();
        let Some(popup) = popup.as_mut().filter(|popup| popup.groups == menu) else {
            return;
        };
        let groups = self.app.ui.borrow().groups.clone();
        unsafe {
            while GetMenuItemCount(menu) > 0 {
                DeleteMenu(menu, 0, MF_BYPOSITION);
            }
        }
        for entry in super::group_entries(&groups, self.app.menu_enabled()) {
            append(menu, &entry, popup);
        }
    }
    fn timer(&self, id: usize) {
        if id == FIRST_CHECK {
            unsafe {
                KillTimer(self.host, FIRST_CHECK);
                SetTimer(self.host, HOURLY_CHECK, 3_600_000, None);
            }
        }
        (self.app.emit)(Event::Update(update::Event::Due));
    }

    fn make_window(&self) -> Option<HWND> {
        let class = wide(SETTINGS_CLASS);
        let title = wide("FlowBar");
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CLIENT.0,
                CLIENT.1,
                null_mut(),
                null_mut(),
                self.instance,
                null(),
            )
        };
        if hwnd.is_null() {
            return None;
        }
        for control in CONTROLS {
            let (class, style, extended) = match control.kind {
                Kind::Button => ("BUTTON", WS_TABSTOP | BS_PUSHBUTTON as u32, 0),
                Kind::Check => ("BUTTON", WS_TABSTOP | BS_AUTOCHECKBOX as u32, 0),
                Kind::Port => (
                    "EDIT",
                    WS_TABSTOP | ES_NUMBER as u32 | ES_AUTOHSCROLL as u32,
                    WS_EX_CLIENTEDGE,
                ),
                Kind::Wrapped | Kind::Status => ("STATIC", SS_NOPREFIX, 0),
                _ if control.id == VERSION => ("STATIC", SS_NOPREFIX | SS_RIGHT, 0),
                _ => ("STATIC", SS_NOPREFIX | SS_ENDELLIPSIS, 0),
            };
            let (class, text) = (wide(class), wide(control.text));
            let child = unsafe {
                CreateWindowExW(
                    extended,
                    class.as_ptr(),
                    text.as_ptr(),
                    WS_CHILD | WS_VISIBLE | style,
                    0,
                    0,
                    0,
                    0,
                    hwnd,
                    control.id as isize as HMENU,
                    self.instance,
                    null(),
                )
            };
            if control.kind == Kind::Port {
                unsafe { SendMessageW(child, EM_SETLIMITTEXT, 5, 0) };
            }
        }
        set_text(
            unsafe { GetDlgItem(hwnd, VERSION) },
            &format!("版本 {}", update::CURRENT),
        );
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let colors = Colors::system();
        *self.window.borrow_mut() = Some(Window {
            hwnd,
            dpi,
            fonts: Fonts::new(dpi),
            colors,
            window_brush: unsafe { CreateSolidBrush(colors.window) },
            card_brush: unsafe { CreateSolidBrush(colors.card) },
            phase: Phase::Stopped,
            dot_top: 0,
        });
        self.layout(hwnd, None);
        // Opening the window should not start editing the first port field.
        unsafe { SetFocus(GetDlgItem(hwnd, START)) };
        Some(hwnd)
    }
    /// Sizes the window for its DPI, centred on its monitor unless Windows suggests a place.
    fn layout(&self, hwnd: HWND, suggested: Option<RECT>) {
        let Some((dpi, fonts)) = self.window.borrow().as_ref().map(|w| (w.dpi, w.fonts)) else {
            return;
        };
        let scale = |value: i32| scale(value, dpi);
        for control in CONTROLS {
            let font = match control.kind {
                Kind::Bold => fonts.bold,
                Kind::Caption => fonts.caption,
                Kind::Secondary | Kind::Wrapped => fonts.small,
                _ => fonts.normal,
            };
            let [x, y, width, height] = control.rect;
            unsafe {
                let child = GetDlgItem(hwnd, control.id);
                MoveWindow(child, scale(x), scale(y), scale(width), scale(height), 1);
                SendMessageW(child, WM_SETFONT, font as WPARAM, 1);
            }
        }
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: scale(CLIENT.0),
            bottom: scale(CLIENT.1),
        };
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        unsafe { AdjustWindowRectExForDpi(&mut frame, style, 0, 0, dpi) };
        let (width, height) = (frame.right - frame.left, frame.bottom - frame.top);
        let (x, y) = match suggested {
            Some(rect) => (rect.left, rect.top),
            None => {
                let mut monitor: MONITORINFO = unsafe { std::mem::zeroed() };
                monitor.cbSize = size_of::<MONITORINFO>() as u32;
                unsafe {
                    GetMonitorInfoW(
                        MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                        &mut monitor,
                    )
                };
                let work = monitor.rcWork;
                (
                    work.left + (work.right - work.left - width) / 2,
                    work.top + (work.bottom - work.top - height) / 2,
                )
            }
        };
        unsafe {
            SetWindowPos(
                hwnd,
                null_mut(),
                x,
                y,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    }
    /// Centres one or two lines of status on the footer buttons, the dot on the first line.
    fn place_status(&self, hwnd: HWND, message: &str) {
        let Some((dpi, font)) = self
            .window
            .borrow()
            .as_ref()
            .map(|w| (w.dpi, w.fonts.normal))
        else {
            return;
        };
        let scale = |value: i32| scale(value, dpi);
        let [x, _, width, _] = STATUS_RECT;
        let line = text_height(hwnd, font, "国", scale(width));
        let height = text_height(hwnd, font, message, scale(width)).clamp(line, line * 2);
        let top = scale(FOOTER_MIDLINE) - height / 2;
        unsafe {
            MoveWindow(
                GetDlgItem(hwnd, STATUS),
                scale(x),
                top,
                scale(width),
                height,
                1,
            )
        };
        if let Some(window) = self.window.borrow_mut().as_mut() {
            window.dot_top = top + line / 2 - scale(4);
        }
        let footer = RECT {
            left: 0,
            top: scale(FOOTER_MIDLINE - 30),
            right: scale(CLIENT.0),
            bottom: scale(CLIENT.1),
        };
        unsafe {
            InvalidateRect(hwnd, &footer, 0);
            InvalidateRect(GetDlgItem(hwnd, STATUS), null(), 1);
        }
    }
    fn paint(&self, hwnd: HWND, dc: HDC) {
        let state = self.window.borrow();
        let Some(window) = state.as_ref().filter(|w| w.hwnd == hwnd) else {
            return;
        };
        let scale = |value: i32| scale(value, window.dpi);
        unsafe {
            let mut client: RECT = std::mem::zeroed();
            windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client);
            FillRect(dc, &client, window.window_brush);
            let pen = CreatePen(PS_SOLID, 1, window.colors.border);
            let old_pen = SelectObject(dc, pen);
            let old_brush = SelectObject(dc, window.card_brush);
            let radius = scale(16);
            for [x, y, width, height] in CARDS {
                RoundRect(
                    dc,
                    scale(x),
                    scale(y),
                    scale(x + width),
                    scale(y + height),
                    radius,
                    radius,
                );
            }
            let dot = CreateSolidBrush(match window.phase {
                Phase::Running => rgb(52, 199, 89),
                Phase::Starting | Phase::Stopping => rgb(255, 149, 0),
                Phase::Failed => rgb(255, 59, 48),
                Phase::Stopped => window.colors.border,
            });
            SelectObject(dc, dot);
            SelectObject(dc, GetStockObject(NULL_PEN));
            let (x, size) = (scale(20), scale(8));
            Ellipse(
                dc,
                x,
                window.dot_top,
                x + size + 1,
                window.dot_top + size + 1,
            );
            SelectObject(dc, old_pen);
            SelectObject(dc, old_brush);
            DeleteObject(pen);
            DeleteObject(dot);
        }
    }
    /// Text colour and background for labels and check boxes.
    fn control_color(&self, dc: HDC, control: HWND) -> Option<HBRUSH> {
        let state = self.window.borrow();
        let window = state.as_ref()?;
        let id = unsafe { GetDlgCtrlID(control) };
        let spec = CONTROLS.iter().find(|spec| spec.id == id)?;
        let color = match spec.kind {
            Kind::Caption | Kind::Secondary | Kind::Wrapped => window.colors.grey,
            Kind::Status if window.phase == Phase::Failed => rgb(196, 43, 28),
            _ => window.colors.text,
        };
        unsafe {
            SetTextColor(dc, color);
            SetBkMode(dc, TRANSPARENT as i32);
        }
        Some(if spec.card {
            window.card_brush
        } else {
            window.window_brush
        })
    }
    fn command(&self, id: i32, code: u32) {
        match (id, code) {
            (IMPORT, BN_CLICKED) => self.perform(Action::Import),
            (REVEAL, BN_CLICKED) => self.perform(Action::Reveal),
            (TUN | AUTO | AUTO_UPDATE, BN_CLICKED) => self.app.commit(self),
            (LOGIN, BN_CLICKED) => self.perform(Action::Login),
            (UPDATE, BN_CLICKED) => {
                if let Some(action) = self.app.update_action() {
                    self.perform(action);
                }
            }
            (LOGS, BN_CLICKED) => self.perform(Action::Logs),
            (START, BN_CLICKED) => self.perform(Action::Connect),
            // Saving opens an alert on failure, so it waits until the focus has settled.
            (id, EN_KILLFOCUS) if PORTS.contains(&id) => unsafe {
                PostMessageW(self.host, WM_COMMIT, 0, 0);
            },
            // Return in a port field, and Escape.
            (IDOK, _) => self.app.commit(self),
            (IDCANCEL, _) => self.close_window(),
            _ => {}
        }
    }
    /// Commits a typed port, as closing the macOS window does, then closes it.
    fn close_window(&self) {
        let Some(hwnd) = self.settings_hwnd() else {
            return;
        };
        self.app.commit(self);
        unsafe { DestroyWindow(hwnd) };
    }
    fn window_destroyed(&self, hwnd: HWND) {
        let window = {
            let mut state = self.window.borrow_mut();
            if state.as_ref().is_some_and(|w| w.hwnd == hwnd) {
                state.take()
            } else {
                None
            }
        };
        if let Some(window) = window {
            window.fonts.delete();
            unsafe {
                DeleteObject(window.window_brush);
                DeleteObject(window.card_brush);
            }
        }
    }
    fn dpi_changed(&self, hwnd: HWND, dpi: u32, suggested: RECT) {
        let old = {
            let mut state = self.window.borrow_mut();
            let Some(window) = state.as_mut().filter(|w| w.hwnd == hwnd) else {
                return;
            };
            window.dpi = dpi;
            std::mem::replace(&mut window.fonts, Fonts::new(dpi))
        };
        self.layout(hwnd, Some(suggested));
        old.delete();
        self.render();
    }
}

impl Fonts {
    /// The system message font and variants of it, for the window's DPI.
    fn new(dpi: u32) -> Self {
        let mut metrics: NONCLIENTMETRICSW = unsafe { std::mem::zeroed() };
        metrics.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
        let found = unsafe {
            SystemParametersInfoForDpi(
                SPI_GETNONCLIENTMETRICS,
                metrics.cbSize,
                (&raw mut metrics).cast(),
                0,
                dpi,
            )
        } != 0;
        let mut base = metrics.lfMessageFont;
        if !found {
            base = unsafe { std::mem::zeroed() };
            base.lfHeight = -scale(12, dpi);
            for (slot, unit) in base.lfFaceName.iter_mut().zip("Segoe UI".encode_utf16()) {
                *slot = unit;
            }
        }
        let make = |size: f32, semibold: bool| {
            let mut font: LOGFONTW = base;
            font.lfHeight = (base.lfHeight as f32 * size).round() as i32;
            if semibold {
                font.lfWeight = 600;
            }
            unsafe { CreateFontIndirectW(&font) }
        };
        Self {
            normal: make(1.0, false),
            bold: make(1.0, true),
            small: make(0.92, false),
            caption: make(0.92, true),
        }
    }
    fn delete(self) {
        for font in [self.normal, self.bold, self.small, self.caption] {
            unsafe { DeleteObject(font) };
        }
    }
}
impl Colors {
    /// Cards tint the window colour towards the text colour, as on macOS.
    fn system() -> Self {
        use windows_sys::Win32::Graphics::Gdi::{
            COLOR_GRAYTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT, GetSysColor,
        };
        let (window, text) = unsafe { (GetSysColor(COLOR_WINDOW), GetSysColor(COLOR_WINDOWTEXT)) };
        Self {
            text,
            grey: unsafe { GetSysColor(COLOR_GRAYTEXT) },
            window,
            card: blend(window, text, 0.04),
            border: blend(window, text, 0.16),
        }
    }
}

fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    u32::from(red) | u32::from(green) << 8 | u32::from(blue) << 16
}
fn blend(from: COLORREF, to: COLORREF, amount: f32) -> COLORREF {
    let channel = |shift: u32| {
        let (a, b) = ((from >> shift) & 255, (to >> shift) & 255);
        ((a as f32 + (b as f32 - a as f32) * amount).round() as u32) << shift
    };
    channel(0) | channel(8) | channel(16)
}
fn scale(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32 + 48) / 96
}
fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain([0]).collect()
}
fn text(hwnd: HWND) -> String {
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    let mut buffer = vec![0u16; length as usize + 1];
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
}
/// Changing only what differs avoids flicker and spurious change notifications.
fn set_text(hwnd: HWND, value: &str) {
    if text(hwnd) != value {
        unsafe { SetWindowTextW(hwnd, wide(value).as_ptr()) };
    }
}
fn enable(hwnd: HWND, enabled: bool) {
    unsafe { EnableWindow(hwnd, i32::from(enabled)) };
}
fn check(hwnd: HWND, checked: bool) {
    unsafe { SendMessageW(hwnd, BM_SETCHECK, usize::from(checked), 0) };
}
fn text_height(hwnd: HWND, font: HFONT, value: &str, width: i32) -> i32 {
    let value: Vec<u16> = value.encode_utf16().collect();
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: 0,
    };
    unsafe {
        let dc = GetDC(hwnd);
        let old = SelectObject(dc, font);
        DrawTextW(
            dc,
            value.as_ptr(),
            value.len() as i32,
            &mut rect,
            DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
        );
        SelectObject(dc, old);
        ReleaseDC(hwnd, dc);
    }
    rect.bottom
}
fn append(menu: HMENU, entry: &Entry, popup: &mut Popup) {
    // An ampersand marks an access key in menu text.
    let label = |title: &str| wide(title.replace('&', "&&"));
    match entry {
        Entry::Separator => unsafe {
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
        },
        Entry::Item {
            title,
            action,
            enabled,
            checked,
        } => {
            let mut flags = MF_STRING;
            if !enabled {
                flags |= MF_GRAYED;
            }
            if *checked == Some(true) {
                flags |= MF_CHECKED;
            }
            let id = action.as_ref().map_or(0, |action| {
                popup.actions.push(action.clone());
                popup.actions.len()
            });
            unsafe { AppendMenuW(menu, flags, id, label(title).as_ptr()) };
        }
        Entry::Menu {
            title,
            entries,
            groups,
        } => {
            let submenu = unsafe { CreatePopupMenu() };
            for entry in entries {
                append(submenu, entry, popup);
            }
            if *groups {
                popup.groups = submenu;
            }
            unsafe { AppendMenuW(menu, MF_POPUP, submenu as usize, label(title).as_ptr()) };
        }
    }
}
/// Builds a tray icon from straight-alpha pixels.
fn tray_icon(size: u32, running: bool) -> HICON {
    let pixels = icon::pixels(size, running);
    let mut header: BITMAPINFO = unsafe { std::mem::zeroed() };
    header.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: size as i32,
        // Negative: top row first.
        biHeight: -(size as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..unsafe { std::mem::zeroed() }
    };
    unsafe {
        let dc = GetDC(null_mut());
        let mut bits = null_mut();
        let color = CreateDIBSection(dc, &header, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        ReleaseDC(null_mut(), dc);
        if color.is_null() || bits.is_null() {
            return null_mut();
        }
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u32>(), pixels.len());
        // The alpha channel decides transparency; the mask only has to exist.
        let mask_row = size.div_ceil(16) as usize * 2;
        let mask_bits = vec![0u8; mask_row * size as usize];
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, mask_bits.as_ptr().cast());
        let info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}
type TaskDialogIndirect = unsafe extern "system" fn(
    *const TASKDIALOGCONFIG,
    *mut i32,
    *mut i32,
    *mut windows_sys::core::BOOL,
) -> windows_sys::core::HRESULT;
/// Task dialogs exist only in common controls 6, which Windows loads for executables with
/// our manifest; a static import would keep the test executable from starting.
fn task_dialog() -> Option<TaskDialogIndirect> {
    unsafe {
        let module = LoadLibraryW(wide("comctl32.dll").as_ptr());
        if module.is_null() {
            return None;
        }
        let address = GetProcAddress(module, c"TaskDialogIndirect".as_ptr().cast())?;
        Some(std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            TaskDialogIndirect,
        >(address))
    }
}
unsafe extern "system" fn bring_to_front(
    hwnd: HWND,
    notification: u32,
    _wparam: WPARAM,
    _lparam: LPARAM,
    _data: isize,
) -> i32 {
    // Results of background work can arrive while another app is in front.
    if notification as i32 == TDN_CREATED {
        unsafe { SetForegroundWindow(hwnd) };
    }
    0
}

unsafe extern "system" fn host_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = match message {
        WM_WAKE => with_shell(|shell| shell.app.drain(shell)),
        WM_SHOW => with_shell(|shell| shell.show_settings()),
        WM_COMMIT => with_shell(|shell| shell.app.commit(shell)),
        WM_TRAY => with_shell(|shell| shell.tray_event(lparam as u32 & 0xFFFF, wparam)),
        WM_TIMER => with_shell(|shell| shell.timer(wparam)),
        WM_INITMENUPOPUP => with_shell(|shell| shell.init_popup(wparam as HMENU)),
        // Signing out or shutting down ends this process without a quit.
        WM_ENDSESSION if wparam != 0 => with_shell(|shell| shell.app.stop()),
        _ if message != 0 && message == TASKBAR_CREATED.load(Ordering::SeqCst) => {
            with_shell(|shell| shell.add_tray())
        }
        _ => None,
    };
    match handled {
        Some(()) => 0,
        None => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
unsafe extern "system" fn settings_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND => {
            let (id, code) = ((wparam & 0xFFFF) as i32, ((wparam >> 16) & 0xFFFF) as u32);
            with_shell(|shell| shell.command(id, code));
            return 0;
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            if let Some(Some(brush)) =
                with_shell(|shell| shell.control_color(wparam as HDC, lparam as HWND))
            {
                return brush as LRESULT;
            }
        }
        // WM_PAINT covers the whole background, and children are clipped.
        WM_ERASEBKGND => return 1,
        WM_PAINT => unsafe {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut paint);
            with_shell(|shell| shell.paint(hwnd, dc));
            EndPaint(hwnd, &paint);
            return 0;
        },
        // Themed buttons ask for the background behind their rounded corners.
        WM_PRINTCLIENT => {
            with_shell(|shell| shell.paint(hwnd, wparam as HDC));
            return 0;
        }
        WM_CLOSE => {
            with_shell(|shell| shell.close_window());
            return 0;
        }
        // Children are gone by now, so their fonts can be deleted.
        WM_NCDESTROY => {
            with_shell(|shell| shell.window_destroyed(hwnd));
        }
        WM_DPICHANGED => {
            let suggested = unsafe { *(lparam as *const RECT) };
            let dpi = ((wparam >> 16) & 0xFFFF) as u32;
            with_shell(|shell| shell.dpi_changed(hwnd, dpi, suggested));
            return 0;
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

pub fn open(path: &Path, reveal: bool) {
    if reveal {
        let explorer = std::env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("explorer.exe"))
            .unwrap_or_else(|| "explorer.exe".into());
        // Explorer parses its own command line, which Command's quoting does not match.
        let _ = Command::new(explorer)
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn();
        return;
    }
    shell_open(path.as_os_str());
}
pub fn open_url(url: &str) {
    shell_open(url.as_ref());
}
fn shell_open(target: &OsStr) {
    let (verb, target) = (wide("open"), wide(target));
    unsafe {
        ShellExecuteW(
            null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
}

/// The Run entry value: this executable, quoted.
fn command_line() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|path| format!("\"{}\"", path.display()))
}
pub fn login_status() -> Login {
    let mut buffer = [0u16; 2048];
    let mut size = (buffer.len() * 2) as u32;
    let (key, name) = (wide(RUN_KEY), wide("FlowBar"));
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    let value = String::from_utf16_lossy(&buffer[..(size as usize / 2).saturating_sub(1)]);
    // An entry for a FlowBar elsewhere would start that copy, so it does not count.
    if status == 0 && command_line().is_some_and(|ours| ours.eq_ignore_ascii_case(&value)) {
        Login::Enabled
    } else {
        Login::Disabled
    }
}
pub fn toggle_login() -> Result<()> {
    let (key, name) = (wide(RUN_KEY), wide("FlowBar"));
    let status = if login_status() == Login::Enabled {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) }
    } else {
        let value = wide(command_line().unwrap_or_default());
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            )
        }
    };
    ensure!(
        status == 0,
        "无法修改登录项：{}",
        std::io::Error::from_raw_os_error(status as i32)
    );
    Ok(())
}

pub fn run() -> Result<()> {
    let smoke = std::env::args().any(|a| a == "--ui-smoke");
    unsafe {
        // The mutex lives until the process exits; a second launch shows the first one's window.
        if !smoke {
            let name = wide("Local\\dev.local.FlowBar");
            CreateMutexW(null(), 0, name.as_ptr());
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let host = FindWindowW(wide(HOST_CLASS).as_ptr(), null());
                if !host.is_null() {
                    let mut pid = 0;
                    GetWindowThreadProcessId(host, &mut pid);
                    AllowSetForegroundWindow(pid);
                    PostMessageW(host, WM_SHOW, 0, 0);
                }
                return Ok(());
            }
        }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        );
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES,
        });
        let instance = GetModuleHandleW(null());
        // The executable's icon resource, embedded by build.rs.
        let load_icon = |metric| {
            let size = GetSystemMetrics(metric);
            LoadImageW(instance, 1 as _, IMAGE_ICON, size, size, LR_DEFAULTCOLOR) as HICON
        };
        for (name, procedure) in [
            (
                HOST_CLASS,
                host_proc as unsafe extern "system" fn(_, _, _, _) -> _,
            ),
            (SETTINGS_CLASS, settings_proc),
        ] {
            let name = wide(name);
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(procedure),
                hInstance: instance,
                hIcon: load_icon(SM_CXICON),
                hIconSm: load_icon(SM_CXSMICON),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                lpszClassName: name.as_ptr(),
                ..std::mem::zeroed()
            };
            ensure!(RegisterClassExW(&class) != 0, "无法注册窗口类");
        }
        // A hidden top-level window: it receives tray clicks and the TaskbarCreated broadcast.
        let host = CreateWindowExW(
            0,
            wide(HOST_CLASS).as_ptr(),
            wide("FlowBar").as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        ensure!(!host.is_null(), "无法创建窗口");
        HOST.store(host, Ordering::SeqCst);
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            Ordering::SeqCst,
        );
        let icon_size = GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()).max(16) as u32;
        let shell = Rc::new(Win {
            app: App::new(|| {
                let host = HOST.load(Ordering::SeqCst);
                if !host.is_null() {
                    PostMessageW(host, WM_WAKE, 0, 0);
                }
            }),
            host,
            instance,
            icons: [tray_icon(icon_size, false), tray_icon(icon_size, true)],
            window: RefCell::new(None),
            popup: RefCell::new(None),
        });
        SHELL.with(|cell| *cell.borrow_mut() = Some(shell.clone()));
        shell.add_tray();
        shell.app.launch(&*shell);
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            // Tab, Return and Escape in the settings window.
            if let Some(settings) = shell.settings_hwnd() {
                if IsDialogMessageW(settings, &message) != 0 {
                    continue;
                }
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        shell.remove_tray();
        shell.app.stop();
        HOST.store(null_mut(), Ordering::SeqCst);
        SHELL.with(|cell| cell.borrow_mut().take());
        for icon in shell.icons {
            DestroyIcon(icon);
        }
    }
    Ok(())
}
