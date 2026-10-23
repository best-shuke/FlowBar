mod api;
mod config;
mod engine;
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

fn entry() -> Result<()> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("--version") => println!(
            "FlowBar {} (Rust, native AppKit)",
            env!("CARGO_PKG_VERSION")
        ),
        Some("--tun-launch") if arguments.len() == 4 => {
            supervisor::root_launch(arguments[1].parse()?, arguments[2].parse()?, &arguments[3])?
        }
        Some("--tun-supervise") if arguments.len() == 4 => {
            supervisor::root_main(arguments[1].parse()?, arguments[2].parse()?, &arguments[3])?
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
            Some(release) => println!("可以更新到 {}：{}", release.version(), release.html_url),
            None => println!("已是最新版本（{}）。", update::CURRENT),
        },
        Some("--login-status") => {
            objc2::rc::autoreleasepool(|_| println!("{:?}", ui::login_status()))
        }
        Some("--engine-smoke") => engine_smoke()?,
        Some(value)
            if value.starts_with("--")
                && ![
                    "--show-settings",
                    "--snapshot",
                    "--no-autoconnect",
                    "--ui-smoke",
                ]
                .contains(&value) =>
        {
            bail!("未知参数或参数数量无效")
        }
        _ => objc2::rc::autoreleasepool(|_| ui::run())?,
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
