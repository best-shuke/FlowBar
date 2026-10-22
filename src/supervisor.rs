use crate::{api::Api, config, process};
use anyhow::{Context, Result, ensure};
use core_foundation::{
    array::CFArray,
    base::{CFRelease, TCFType},
    boolean::CFBoolean,
    dictionary::CFDictionary,
    number::CFNumber,
    string::CFString,
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, OsStr, c_void},
    fs,
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr,
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub struct Status {
    pub state: String,
    pub core_pid: Option<u32>,
}
pub fn root_session(uid: u32, id: &str) -> PathBuf {
    PathBuf::from(format!(
        "/Library/Application Support/FlowBar/{uid}/Sessions/{id}"
    ))
}
pub fn status(path: &Path, state: &str, pid: Option<u32>) -> Result<()> {
    config::atomic_write(
        &path.join("status.json"),
        &serde_json::to_vec(&Status {
            state: state.into(),
            core_pid: pid,
        })?,
        0o644,
    )
}
pub fn read_status(path: &Path) -> Option<Status> {
    serde_json::from_slice(&fs::read(path.join("status.json")).ok()?).ok()
}
/// A session still owns its directories while its core process is alive.
pub fn running(path: &Path) -> bool {
    read_status(path).is_some_and(|s| s.state == "running" && s.core_pid.is_some_and(is_core))
}
/// After a crash or reboot the recorded PID can belong to any process, so it must still be a mihomo.
fn is_core(pid: u32) -> bool {
    let mut buffer = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length =
        unsafe { libc::proc_pidpath(pid as i32, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 {
        // Keep the session if the process exists but its path is hidden from us.
        return io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    }
    Path::new(OsStr::from_bytes(&buffer[..length as usize])).file_name()
        == Some(OsStr::new("mihomo"))
}
pub fn resources() -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    Ok(executable
        .parent()
        .and_then(Path::parent)
        .context("应用包路径无效")?
        .join("Resources"))
}

fn root_paths(uid: u32, id: &str) -> Result<(PathBuf, PathBuf, PathBuf)> {
    ensure!(
        unsafe { libc::geteuid() } == 0 && uid > 0,
        "TUN 需要管理员权限。"
    );
    ensure!(Uuid::parse_str(id)?.to_string() == id, "会话编号无效");
    let account = unsafe { libc::getpwuid(uid) };
    ensure!(!account.is_null(), "找不到用户账户");
    let user_home = PathBuf::from(unsafe { CStr::from_ptr((*account).pw_dir) }.to_str()?);
    let user_data = user_home.join("Library/Application Support/FlowBar");
    let base = PathBuf::from(format!("/Library/Application Support/FlowBar/{uid}"));
    let session = root_session(uid, id);
    for dir in [
        base.parent().unwrap(),
        &base,
        &base.join("Sessions"),
        &session,
    ] {
        root_directory(dir, 0o711)?;
    }
    let cache = base.join("Data");
    root_directory(&cache, 0o700)?;
    Ok((user_data, session, cache))
}

fn root_log(session: &Path, name: &str, uid: u32) -> Result<fs::File> {
    let log = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(session.join(name))?;
    ensure!(
        unsafe { libc::fchown(log.as_raw_fd(), uid, !0) } == 0,
        "无法设置日志文件权限"
    );
    Ok(log)
}

/// A foreground authorization command returns only after the detached supervisor
/// has made the core ready. Early errors remain visible to osascript and the GUI.
pub fn root_launch(parent: i32, uid: u32, id: &str) -> Result<()> {
    ensure!(parent > 1, "父进程无效");
    let (_, session, _) = root_paths(uid, id)?;
    let log = root_log(&session, "bootstrap.log", uid)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--tun-supervise")
        .arg(parent.to_string())
        .arg(uid.to_string())
        .arg(id)
        .current_dir("/")
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    let mut child = process::spawn_detached(&mut command)?;
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if read_status(&session).is_some_and(|s| s.state == "running") {
            child.detach();
            return Ok(());
        }
        if read_status(&session).is_some_and(|s| s.state == "failed")
            || child.0.try_wait()?.is_some()
        {
            let detail = ["core.log", "bootstrap.log"]
                .into_iter()
                .filter_map(|name| fs::read_to_string(session.join(name)).ok())
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!("管理员内核启动失败：{}", detail.trim());
        }
        ensure!(
            Instant::now() < deadline,
            "管理员内核启动超时，请查看启动日志。"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

pub fn root_main(parent: i32, uid: u32, id: &str) -> Result<()> {
    let (user_data, session, cache) = root_paths(uid, id)?;
    let sessions = session.parent().unwrap();
    config::prune_sessions(sessions, |other| {
        other == id || running(&sessions.join(other))
    });
    let stage = user_data.join("Sessions").join(id);
    let mut log = root_log(&session, "core.log", uid)?;
    let core = session.join("mihomo");
    let root_config = session.join("config.yaml");
    struct Cleanup(PathBuf, PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
            let _ = fs::remove_file(&self.1);
        }
    }
    let _cleanup = Cleanup(core.clone(), root_config.clone());
    let result = (|| {
        let mut source = open_regular(&stage.join("config.yaml"), Some(uid))?;
        let mut bytes = String::new();
        io::Read::read_to_string(&mut source, &mut bytes)?;
        let tun = config::parse(&bytes)?["tun"]["enable"]
            .as_bool()
            .unwrap_or(false);
        config::private_write(&root_config, bytes.as_bytes())?;
        copy_regular(&resources()?.join("mihomo"), &core, None)?;
        fs::set_permissions(&core, fs::Permissions::from_mode(0o700))?;
        copy_data(&user_data.join("Data"), &cache, uid)?;
        run_core(
            &core,
            &root_config,
            &cache,
            &stage,
            &session,
            parent,
            &log,
            tun,
            &bytes,
        )
    })();
    if let Err(ref error) = result {
        let _ = writeln!(log, "{error:#}");
        let _ = status(&session, "failed", None);
    }
    result
}

pub fn user_main(parent: i32, id: &str) -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "普通模式请使用普通用户运行"
    );
    ensure!(Uuid::parse_str(id)?.to_string() == id, "会话编号无效");
    let stage = config::home().join("Sessions").join(id);
    let mut log = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(stage.join("core.log"))?;
    let config = stage.join("config.yaml");
    let yaml = fs::read_to_string(&config)?;
    let result = run_core(
        &resources()?.join("mihomo"),
        &config,
        &config::home().join("Data"),
        &stage,
        &stage,
        parent,
        &log,
        false,
        &yaml,
    );
    if let Err(ref error) = result {
        let _ = writeln!(log, "{error:#}");
        let _ = status(&stage, "failed", None);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn run_core(
    core: &Path,
    config_file: &Path,
    cache: &Path,
    stage: &Path,
    session: &Path,
    parent: i32,
    log: &fs::File,
    tun: bool,
    yaml: &str,
) -> Result<()> {
    let parsed = config::parse(yaml)?;
    let port: u16 = parsed["external-controller"]
        .as_str()
        .context("控制端口缺失")?
        .rsplit(':')
        .next()
        .unwrap()
        .parse()?;
    let secret = parsed["secret"]
        .as_str()
        .context("控制密钥缺失")?
        .to_owned();
    let api = Api { port, secret };
    let mut command = Command::new(core);
    command
        .args([
            "-d",
            cache.to_str().unwrap(),
            "-f",
            config_file.to_str().unwrap(),
        ])
        .current_dir(cache)
        .env_clear()
        .env("HOME", if tun { Path::new("/var/root") } else { cache })
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("GOGC", "50")
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?));
    let mut dns = None;
    process::supervise(
        command,
        parent,
        &stage.join("stop"),
        Some(log),
        |pid, cancelled| {
            let mut ready = false;
            for _ in 0..100 {
                ensure!(
                    !cancelled.load(Ordering::SeqCst)
                        && !stage.join("stop").exists()
                        && unsafe { libc::kill(parent, 0) } == 0,
                    "连接已取消"
                );
                if api.request("GET", "/version", None).is_ok() {
                    ready = true;
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            ensure!(ready, "内核启动超时，请检查端口或配置。");
            // mihomo has loaded the config; drop the copy holding credentials and the secret.
            let _ = fs::remove_file(config_file);
            if tun {
                dns = Some(SessionDns::new(
                    session.file_name().unwrap().to_str().unwrap(),
                    config::dns_port(yaml)?,
                )?);
            }
            status(session, "running", Some(pid))
        },
    )?;
    drop(dns);
    status(session, "stopped", None)?;
    Ok(())
}

fn root_directory(path: &Path, mode: u32) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.uid() == 0 && metadata.is_dir(),
            "TUN 目录的所有权或类型无效：{}",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(path)?,
        Err(error) => return Err(error.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
fn open_regular(path: &Path, owner: Option<u32>) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && owner.is_none_or(|uid| metadata.uid() == uid),
        "文件类型或所有权无效：{}",
        path.display()
    );
    Ok(file)
}
fn copy_regular(source: &Path, target: &Path, owner: Option<u32>) -> Result<()> {
    let mut input = open_regular(source, owner)?;
    let temporary = target.with_file_name(format!(".copy-{}", Uuid::new_v4()));
    let result = (|| {
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        io::copy(&mut input, &mut output)?;
        // Keep the source age so mihomo refreshes providers on their real schedule.
        output.set_modified(input.metadata()?.modified()?)?;
        fs::rename(&temporary, target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
fn copy_data(source: &Path, destination: &Path, owner: u32) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink() && metadata.uid() == owner,
            "数据目录存在符号链接或所有权不符。"
        );
        let target = destination.join(entry.file_name());
        if metadata.is_dir() {
            root_directory(&target, 0o700)?;
            copy_data(&path, &target, owner)?;
        } else {
            // The status change time moves whenever a file is written or replaced, even when
            // its mtime is preserved or set back. Keep a root copy written after the source
            // last changed: either our copy or the root core's own refresh of it.
            let unchanged = fs::symlink_metadata(&target).ok().is_some_and(|dest| {
                dest.is_file()
                    && dest.uid() == 0
                    && (metadata.ctime(), metadata.ctime_nsec())
                        <= (dest.ctime(), dest.ctime_nsec())
            });
            if !unchanged {
                copy_regular(&path, &target, Some(owner))?;
            }
        }
    }
    Ok(())
}

type DynamicStore = *const c_void;
#[link(name = "SystemConfiguration", kind = "framework")]
unsafe extern "C" {
    static kSCDynamicStoreUseSessionKeys: *const c_void;
    fn SCDynamicStoreCreateWithOptions(
        allocator: *const c_void,
        name: *const c_void,
        options: *const c_void,
        callback: *const c_void,
        context: *mut c_void,
    ) -> DynamicStore;
    fn SCDynamicStoreSetValue(store: DynamicStore, key: *const c_void, value: *const c_void) -> u8;
    fn SCDynamicStoreRemoveValue(store: DynamicStore, key: *const c_void) -> u8;
}
struct SessionDns {
    store: DynamicStore,
    key: CFString,
}
impl SessionDns {
    fn new(id: &str, port: u16) -> Result<Self> {
        let session_key =
            unsafe { CFString::wrap_under_get_rule(kSCDynamicStoreUseSessionKeys.cast()) };
        let options = CFDictionary::from_CFType_pairs(&[(
            session_key.as_CFType(),
            CFBoolean::true_value().as_CFType(),
        )]);
        let name = CFString::new("FlowBar Rust TUN DNS");
        let store = unsafe {
            SCDynamicStoreCreateWithOptions(
                ptr::null(),
                name.as_CFTypeRef(),
                options.as_CFTypeRef(),
                ptr::null(),
                ptr::null_mut(),
            )
        };
        ensure!(!store.is_null(), "无法创建临时 DNS 会话");
        let result = Self {
            store,
            key: CFString::new(&format!("State:/Network/Service/FlowBar-{id}/DNS")),
        };
        let addresses = CFArray::from_CFTypes(&[CFString::new("127.0.0.1")]);
        let domains = CFArray::from_CFTypes(&[CFString::new("")]);
        let orders = CFArray::from_CFTypes(&[CFNumber::from(1)]);
        let values = [
            ("ServerAddresses", addresses.as_CFType()),
            ("ServerPort", CFNumber::from(i32::from(port)).as_CFType()),
            ("SupplementalMatchDomains", domains.as_CFType()),
            ("SupplementalMatchOrders", orders.as_CFType()),
            (
                "SupplementalMatchDomainsNoSearch",
                CFNumber::from(1).as_CFType(),
            ),
            ("SearchOrder", CFNumber::from(1).as_CFType()),
        ];
        let pairs: Vec<_> = values
            .into_iter()
            .map(|(key, value)| (CFString::new(key).as_CFType(), value))
            .collect();
        let dictionary = CFDictionary::from_CFType_pairs(&pairs);
        ensure!(
            unsafe {
                SCDynamicStoreSetValue(store, result.key.as_CFTypeRef(), dictionary.as_CFTypeRef())
            } != 0,
            "无法设置临时 TUN DNS"
        );
        Ok(result)
    }
}
impl Drop for SessionDns {
    fn drop(&mut self) {
        unsafe {
            SCDynamicStoreRemoveValue(self.store, self.key.as_CFTypeRef());
            CFRelease(self.store);
        }
    }
}
