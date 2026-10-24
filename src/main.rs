// A tray app has no console window; command-line use attaches to the caller's console.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod api;
mod config;
mod engine;
mod geo;
mod process;
mod supervisor;
mod ui;
mod update;

use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
const TOOLKIT: &str = "native AppKit";
#[cfg(windows)]
const TOOLKIT: &str = "native Win32";
#[cfg(target_os = "linux")]
const TOOLKIT: &str = "GTK 4";

fn entry() -> Result<()> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    if arguments
        .first()
        .is_some_and(|a| !ui::ARGUMENTS.contains(&a.as_str()))
    {
        console::attach();
    }
    match arguments.first().map(String::as_str) {
        Some("--version") => println!("FlowBar {} (Rust, {TOOLKIT})", env!("CARGO_PKG_VERSION")),
        #[cfg(unix)]
        Some("--tun-launch") if arguments.len() == 4 => {
            supervisor::root_launch(arguments[1].parse()?, arguments[2].parse()?, &arguments[3])?
        }
        #[cfg(unix)]
        Some("--tun-supervise") if arguments.len() == 4 => {
            supervisor::root_main(arguments[1].parse()?, arguments[2].parse()?, &arguments[3])?
        }
        #[cfg(windows)]
        Some("--tun-launch") if arguments.len() == 3 => {
            supervisor::root_launch(arguments[1].parse()?, Path::new(&arguments[2]))?
        }
        #[cfg(windows)]
        Some("--tun-supervise") if arguments.len() == 3 => {
            supervisor::root_main(arguments[1].parse()?, Path::new(&arguments[2]))?
        }
        Some("--user-supervise") if arguments.len() == 3 => {
            supervisor::user_main(arguments[1].parse()?, &arguments[2])?
        }
        Some("--import") if arguments.len() >= 2 => {
            let settings = config::import_profile(
                Path::new(&arguments[1]),
                arguments.get(2).map(String::as_str),
            )?;
            let summary =
                config::summary(&fs::read_to_string(config::home().join("profile.yaml"))?)?;
            println!(
                "已导入 {}：{} 个节点，{} 个策略组，{} 条规则。",
                settings.profile_name,
                summary.proxies,
                summary.groups.len(),
                summary.rules
            );
        }
        Some("--render") if arguments.len() >= 3 => {
            let settings = config::Settings {
                tun: arguments.iter().any(|s| s == "--tun"),
                ..config::Settings::default()
            };
            let yaml = config::runtime(
                &fs::read_to_string(&arguments[1])?,
                &settings,
                "flowbar-local-smoke-test-token",
            )?;
            config::private_write(Path::new(&arguments[2]), yaml.as_bytes())?;
            println!("运行配置已生成；TUN={}。", settings.tun);
        }
        Some("--check-update") => match update::check(None)? {
            update::Check::Available(release) => {
                println!("可以更新到 {}：{}", release.version(), release.html_url)
            }
            update::Check::Current => println!("已是最新版本（{}）。", update::CURRENT),
            update::Check::Unavailable => bail!(update::UNAVAILABLE),
        },
        Some("--login-status") => println!("{:?}", ui::login_status()),
        Some("--engine-smoke") => engine_smoke()?,
        // The new version after an in-app update, started by the old one.
        #[cfg(not(target_os = "macos"))]
        Some("--updated") if arguments.len() == 3 => {
            update::finish(arguments[1].parse()?, Path::new(&arguments[2]));
            ui::run()?
        }
        Some(value) if value.starts_with("--") && !ui::ARGUMENTS.contains(&value) => {
            bail!("未知参数或参数数量无效")
        }
        _ => ui::run()?,
    }
    Ok(())
}

fn engine_smoke() -> Result<()> {
    let listeners = (0..3)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0"))
        .collect::<std::io::Result<Vec<_>>>()?;
    let ports = listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect::<Vec<_>>();
    let settings = config::Settings {
        tun: false,
        proxy_port: ports[0],
        controller_port: ports[1],
        dns_port: ports[2],
        ..config::Settings::load()
    };
    drop(listeners);
    let (sender, receiver) = std::sync::mpsc::channel();
    let run = engine::start(
        settings,
        Arc::new(move |event| {
            let _ = sender.send(event);
        }),
    )?;
    struct Stop(Arc<engine::Run>);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let _cleanup = Stop(run.clone());
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut connected = false;
    loop {
        let event = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("进程管理测试超时")?;
        match event {
            engine::Event::Progress(_, message) => println!("INFO {message}"),
            engine::Event::Ready(_, groups) => {
                println!("PASS Rust engine started, {} strategy groups", groups.len());
                connected = true;
                run.cancel();
            }
            engine::Event::Stopped(_) if connected => {
                println!(
                    "PASS Rust engine stopped and reaped its supervisor/core; TUN stayed disabled"
                );
                return Ok(());
            }
            engine::Event::Failed(_, message) | engine::Event::Error(message) => bail!(message),
            engine::Event::Stopped(_) => bail!("内核启动前退出"),
            _ => {}
        }
    }
}

fn main() {
    if let Err(error) = entry() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

/// A GUI-subsystem program has no standard output of its own on Windows.
#[cfg(windows)]
mod console {
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{CreateFileW, FILE_SHARE_WRITE, OPEN_EXISTING},
        System::Console::{
            ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE,
            STD_OUTPUT_HANDLE, SetStdHandle,
        },
    };
    /// Prints to the console of the command prompt that started us, unless output is
    /// already redirected, for example to a pipe of the update check.
    pub fn attach() {
        unsafe {
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            if !(output.is_null() || output == INVALID_HANDLE_VALUE)
                || AttachConsole(ATTACH_PARENT_PROCESS) == 0
            {
                return;
            }
            let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
            let console = CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if console != INVALID_HANDLE_VALUE {
                SetStdHandle(STD_OUTPUT_HANDLE, console);
                SetStdHandle(STD_ERROR_HANDLE, console);
            }
        }
    }
}
