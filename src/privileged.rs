//! One-time authorized, root-owned TUN helper for unsigned macOS builds.
//! The protocol only accepts a session UUID. UID/PID come from the socket, and
//! both endpoints verify their peer; no caller-supplied program or shell is run.
use crate::{config, engine::Run, process, supervisor};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};

const BASE: &str = "/Library/Application Support/FlowBar";
#[derive(Serialize, Deserialize)]
enum Request {
    Ping,
    Start(String),
}
#[derive(Serialize, Deserialize)]
struct Response {
    error: Option<String>,
}
fn base(uid: u32) -> PathBuf {
    PathBuf::from(BASE).join(uid.to_string())
}
fn helper(uid: u32) -> PathBuf {
    base(uid).join("Helper/FlowBar.app/Contents/MacOS/FlowBar")
}
fn socket(uid: u32) -> PathBuf {
    base(uid).join("helper.sock")
}
fn label(uid: u32) -> String {
    format!("dev.local.FlowBar.tun.{uid}")
}
fn plist(uid: u32) -> PathBuf {
    PathBuf::from("/Library/LaunchDaemons").join(format!("{}.plist", label(uid)))
}
fn digest(path: &Path) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash.finalize().to_vec())
}
fn peer(stream: &UnixStream) -> Result<(u32, u32)> {
    let (mut uid, mut gid) = (0, 0);
    ensure!(
        unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0,
        "无法验证后台服务账户"
    );
    let mut pid: libc::pid_t = 0;
    let mut length = size_of::<libc::pid_t>() as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                0,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &mut length,
            )
        } == 0
            && pid > 1,
        "无法验证后台服务进程"
    );
    Ok((uid, pid as u32))
}

/// Validate the running peer's code, bound to its audit token (including PID generation).
/// Checking a hash of the executable on disk alone would allow a path replacement race.
fn signature_matches(stream: &UnixStream) -> Result<()> {
    use core_foundation::{
        base::{CFType, TCFType},
        data::CFData,
        dictionary::CFDictionary,
        string::CFString,
    };
    use std::{ffi::c_void, ptr};
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecGuestAttributeAudit: *const c_void;
        fn SecCodeCopySelf(flags: u32, code: *mut *const c_void) -> i32;
        fn SecCodeCopyDesignatedRequirement(
            code: *const c_void,
            flags: u32,
            requirement: *mut *const c_void,
        ) -> i32;
        fn SecCodeCopyGuestWithAttributes(
            host: *const c_void,
            attributes: *const c_void,
            flags: u32,
            code: *mut *const c_void,
        ) -> i32;
        fn SecCodeCheckValidity(code: *const c_void, flags: u32, requirement: *const c_void)
        -> i32;
    }
    let mut token = [0u8; 32];
    let mut length = token.len() as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                0,
                libc::LOCAL_PEERTOKEN,
                token.as_mut_ptr().cast(),
                &mut length,
            )
        } == 0
            && length == 32,
        "无法读取客户端审计身份"
    );
    let key = unsafe { CFString::wrap_under_get_rule(kSecGuestAttributeAudit.cast()) };
    let attributes = CFDictionary::from_CFType_pairs(&[(
        key.as_CFType(),
        CFData::from_buffer(&token).as_CFType(),
    )]);
    let mut raw = ptr::null();
    ensure!(
        unsafe { SecCodeCopySelf(0, &mut raw) } == 0,
        "无法读取应用代码身份"
    );
    let own = unsafe { CFType::wrap_under_create_rule(raw) };
    ensure!(
        unsafe { SecCodeCopyDesignatedRequirement(own.as_CFTypeRef(), 0, &mut raw) } == 0,
        "无法读取应用签名要求"
    );
    let requirement = unsafe { CFType::wrap_under_create_rule(raw) };
    ensure!(
        unsafe {
            SecCodeCopyGuestWithAttributes(ptr::null(), attributes.as_CFTypeRef(), 0, &mut raw)
        } == 0,
        "无法读取对端代码身份"
    );
    let guest = unsafe { CFType::wrap_under_create_rule(raw) };
    ensure!(
        unsafe { SecCodeCheckValidity(guest.as_CFTypeRef(), 0, requirement.as_CFTypeRef()) } == 0,
        "对端应用代码签名不符"
    );
    Ok(())
}
fn request(uid: u32, request: Request, identity: &[u8]) -> Result<()> {
    let mut stream = UnixStream::connect(socket(uid))?;
    stream.set_read_timeout(Some(Duration::from_secs(50)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let (owner, pid) = peer(&stream)?;
    ensure!(
        owner == 0 && digest(&supervisor::executable(pid)?)? == identity,
        "后台服务身份或版本不符"
    );
    signature_matches(&stream)?;
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(256 * 1024)
        .read_line(&mut line)?;
    let response: Response = serde_json::from_str(&line).context("后台服务响应无效")?;
    if let Some(error) = response.error {
        anyhow::bail!("{error}");
    }
    Ok(())
}

pub fn launch(run: &Run) -> Result<()> {
    let uid = unsafe { libc::getuid() };
    let identity = digest(&std::env::current_exe()?)?;
    if request(uid, Request::Ping, &identity).is_err() {
        // An existing launchd job may still be starting after login or a crash.
        let deadline = Instant::now() + Duration::from_secs(2);
        while helper(uid).is_file() && Instant::now() < deadline {
            if request(uid, Request::Ping, &identity).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        if request(uid, Request::Ping, &identity).is_err() {
            run.diagnostic("helper: installing/upgrading the root-owned launchd service; requesting authorization");
            let command = format!(
                "{} --install-tun-helper {uid}",
                process::shell_quote(
                    std::env::current_exe()?
                        .to_str()
                        .context("应用路径编码无效")?
                )
            );
            let script = format!(
                "do shell script {} with administrator privileges",
                process::apple_string(&command)
            );
            let mut authorize = Command::new("/usr/bin/osascript");
            authorize.args(["-e", &script]);
            let output = process::command_logged(
                authorize,
                Duration::from_secs(180),
                &run.cancelled,
                &run.command_pid,
                &run.stage.join("authorization.log"),
                |_| {},
            )?;
            ensure!(
                output.status.success(),
                "后台服务安装失败或授权已取消，请查看 authorization.log。"
            );
        }
    }
    ensure!(!run.cancelled.load(Ordering::SeqCst), "操作已取消");
    request(uid, Request::Ping, &identity)
        .context("无法连接 TUN 后台服务，请检查系统设置中的后台项目")?;
    run.diagnostic(
        "helper: authenticated service; starting session without another password prompt",
    );
    request(uid, Request::Start(run.id.clone()), &identity)
}

/// Called only by the explicit macOS administrator authorization command.
pub fn install(uid: u32) -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } == 0 && uid > 0,
        "安装后台服务需要管理员权限"
    );
    let destination = helper(uid);
    let resources = destination
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("Resources");
    for directory in [
        PathBuf::from(BASE),
        base(uid),
        base(uid).join("Helper"),
        base(uid).join("Helper/FlowBar.app"),
        base(uid).join("Helper/FlowBar.app/Contents"),
        destination.parent().unwrap().to_path_buf(),
        resources.clone(),
    ] {
        supervisor::root_directory(&directory, 0o755)?;
    }
    for (source, target, mode) in [
        (std::env::current_exe()?, destination.clone(), 0o755),
        (supervisor::core()?, resources.join(supervisor::CORE), 0o700),
    ] {
        config::atomic_write(&target, &fs::read(&source)?, mode)?;
    }
    // Keep the app's signature metadata so Security can validate its running identity.
    let source_contents = std::env::current_exe()?
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let contents = destination.parent().unwrap().parent().unwrap();
    config::atomic_write(
        &contents.join("Info.plist"),
        &fs::read(source_contents.join("Info.plist"))?,
        0o644,
    )?;
    let signatures = contents.join("_CodeSignature");
    supervisor::root_directory(&signatures, 0o755)?;
    config::atomic_write(
        &signatures.join("CodeResources"),
        &fs::read(source_contents.join("_CodeSignature/CodeResources"))?,
        0o644,
    )?;
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{}</string>
<key>ProgramArguments</key><array><string>{}</string><string>--tun-helper</string><string>{uid}</string></array>
<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
<key>ThrottleInterval</key><integer>5</integer>
<key>ProcessType</key><string>Background</string>
<key>Umask</key><integer>63</integer>
</dict></plist>"#,
        label(uid),
        destination.display()
    );
    config::atomic_write(&plist(uid), document.as_bytes(), 0o644)?;
    let service = format!("system/{}", label(uid));
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &service])
        .output();
    let output = Command::new("/bin/launchctl")
        .arg("bootstrap")
        .arg("system")
        .arg(plist(uid))
        .output()?;
    ensure!(
        output.status.success(),
        "无法加载后台服务：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let identity = digest(&destination)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if request(uid, Request::Ping, &identity).is_ok() {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "后台服务启动超时，请检查系统设置中的后台项目"
        );
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

pub fn serve(uid: u32) -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } == 0 && uid > 0,
        "后台服务需要管理员权限"
    );
    supervisor::root_directory(&base(uid), 0o755)?;
    let path = socket(uid);
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // Root can probe readiness; only the installed user can send Start requests.
    let path_c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    ensure!(
        unsafe { libc::chown(path_c.as_ptr(), uid, !0) } == 0,
        "无法设置后台服务权限"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let identity = digest(&std::env::current_exe()?)?;
    for mut stream in listener.incoming().flatten() {
        let result = (|| -> Result<()> {
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            let (owner, pid) = peer(&stream)?;
            ensure!(
                (owner == uid || owner == 0) && digest(&supervisor::executable(pid)?)? == identity,
                "客户端身份无效"
            );
            signature_matches(&stream)?;
            let mut line = String::new();
            BufReader::new(&stream).take(4096).read_line(&mut line)?;
            match serde_json::from_str::<Request>(&line)? {
                Request::Ping => Ok(()),
                Request::Start(id) => {
                    ensure!(owner == uid, "会话账户不符");
                    ensure!(
                        uuid::Uuid::parse_str(&id)?.to_string() == id,
                        "会话编号无效"
                    );
                    supervisor::root_launch(pid, uid, &id)
                }
            }
        })();
        let response = Response {
            error: result.err().map(|error| format!("{error:#}")),
        };
        let _ = serde_json::to_writer(&mut stream, &response);
        let _ = stream.write_all(b"\n");
    }
    Ok(())
}

/// Run against an already installed helper. No authorization fallback, TUN, or DNS changes.
pub fn smoke() -> Result<()> {
    let uid = unsafe { libc::getuid() };
    ensure!(uid > 0, "使用普通账户运行测试");
    let identity = digest(&std::env::current_exe()?)?;
    request(uid, Request::Ping, &identity)?;
    ensure!(
        request(uid, Request::Start("../invalid".into()), &identity).is_err(),
        "服务接受了无效会话"
    );
    config::private_dir(&config::home().join("Data"))?;
    for valid in [false, true, true] {
        let id = uuid::Uuid::new_v4().to_string();
        let stage = config::home().join("Sessions").join(&id);
        config::private_dir(&stage)?;
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = config::private_write(&self.0.join("stop"), &[]);
            }
        }
        let _cleanup = Cleanup(stage.clone());
        let listeners = (0..2)
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0"))
            .collect::<std::io::Result<Vec<_>>>()?;
        let ports = listeners
            .iter()
            .map(|l| l.local_addr().unwrap().port())
            .collect::<Vec<_>>();
        let secret = uuid::Uuid::new_v4().to_string();
        if valid {
            config::private_write(&stage.join("config.yaml"), format!("mixed-port: {}\nexternal-controller: 127.0.0.1:{}\nsecret: {secret}\nlog-level: debug\ntun: {{enable: false}}\ndns: {{enable: false}}\nproxies: []\nproxy-groups: []\nrules: [MATCH,DIRECT]\n", ports[0], ports[1]).as_bytes())?;
        }
        drop(listeners);
        let result = request(uid, Request::Start(id.clone()), &identity);
        let session = supervisor::root_session(uid, &id);
        if !valid {
            let error = result.expect_err("missing config must fail").to_string();
            ensure!(
                error.contains("管理员内核启动失败"),
                "启动错误未传回：{error}"
            );
        } else {
            result?;
            let api = crate::api::Api {
                port: ports[1],
                secret,
            };
            let config = api.request("GET", "/configs", None)?;
            ensure!(config["tun"]["enable"] == false, "测试不能开启 TUN");
            config::private_write(&stage.join("stop"), &[])?;
            let deadline = Instant::now() + Duration::from_secs(10);
            while supervisor::read_status(&session).is_none_or(|s| s.state != "stopped") {
                ensure!(Instant::now() < deadline, "测试内核未停止");
                thread::sleep(Duration::from_millis(100));
            }
            while session.join("config.yaml").exists() || session.join(supervisor::CORE).exists() {
                ensure!(Instant::now() < deadline, "测试内核未清理");
                thread::sleep(Duration::from_millis(50));
            }
            ensure!(
                !session.join("config.yaml").exists() && !session.join(supervisor::CORE).exists(),
                "测试内核未清理"
            );
        }
        let _ = fs::remove_dir_all(stage);
    }
    println!(
        "PASS persistent TUN helper: authenticated ping, invalid UUID rejected, startup errors, repeated connect/stop without authorization"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "subprocess endpoint for the code identity test"]
    fn signature_peer() {
        let path = std::env::var("FLOWBAR_SIGNATURE_TEST_SOCKET").unwrap();
        let stream = UnixStream::connect(path).unwrap();
        signature_matches(&stream).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut byte = [0];
        (&stream).read_exact(&mut byte).unwrap();
    }
    #[test]
    fn authenticates_running_code_and_rejects_an_unrelated_process() {
        let path = PathBuf::from(format!("/tmp/flowbar-peer-{}.sock", uuid::Uuid::new_v4()));
        let listener = UnixListener::bind(&path).unwrap();
        let mut child = process::ManagedChild::new(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "privileged::tests::signature_peer", "--ignored"])
                .env("FLOWBAR_SIGNATURE_TEST_SOCKET", &path)
                .spawn()
                .unwrap(),
        );
        let (mut stream, _) = listener.accept().unwrap();
        signature_matches(&stream).unwrap();
        stream.write_all(b"1").unwrap();
        assert!(child.0.wait().unwrap().success());
        let mut unrelated = process::ManagedChild::new(Command::new("/usr/bin/python3")
            .args(["-c", "import socket,sys,time; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); time.sleep(10)"])
            .arg(&path).spawn().unwrap());
        let (stream, _) = listener.accept().unwrap();
        assert!(signature_matches(&stream).is_err());
        unrelated.stop();
        fs::remove_file(path).unwrap();
    }
}
