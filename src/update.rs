use crate::{process, supervisor};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use uuid::Uuid;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const STAGING: &str = ".FlowBar-update-";
/// The installed unit an update replaces, as packed in the release archive.
#[cfg(target_os = "macos")]
const BUNDLE: &str = "FlowBar.app";
#[cfg(not(target_os = "macos"))]
const BUNDLE: &str = "FlowBar";
/// The executable inside the bundle.
#[cfg(target_os = "macos")]
const EXECUTABLE: &str = "Contents/MacOS/FlowBar";
#[cfg(target_os = "linux")]
const EXECUTABLE: &str = "flowbar";
#[cfg(windows)]
const EXECUTABLE: &str = "FlowBar.exe";
/// Waits for the old process to exit, because a second FlowBar only forwards to the first,
/// then removes the previous version that the swap left in the staging directory.
#[cfg(target_os = "macos")]
const RELAUNCH: &str = r#"i=0; while kill -0 "$1" 2>/dev/null && [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done; rm -rf "$3"; exec /usr/bin/open "$2""#;

#[derive(Debug)]
pub enum Event {
    Due,
    Checked(bool, Result<Check, String>),
    Staged(Result<PathBuf, String>),
}
#[derive(Debug)]
pub enum Check {
    Current,
    Available(Release),
    /// Anonymous GitHub requests cannot distinguish private repos from missing releases.
    Unavailable,
}
pub const UNAVAILABLE: &str = "更新源不可用（GitHub HTTP 404）：仓库不可公开访问，或尚无正式 Release。\
    内置更新仅支持公开仓库的正式 Release，不支持私有仓库认证，也不会使用 Git 的 SSH 权限。\
    私有部署请手动拉取源码并重新构建；如需内置更新，请公开仓库并发布对应平台的安装包。\
    可关闭「自动检查更新」。这不影响代理连接。";
#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    tag_name: String,
    pub html_url: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    assets: Vec<Asset>,
}
#[derive(Clone, Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}
impl Release {
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }
}

/// The newest published release when it is newer than this build.
pub fn check(proxy: Option<u16>) -> Result<Check> {
    let repository = env!("CARGO_PKG_REPOSITORY").trim_start_matches("https://github.com/");
    let (status, body) = curl(
        &format!("https://api.github.com/repos/{repository}/releases/latest"),
        proxy,
        &[
            "--max-time",
            "30",
            "--header",
            "Accept: application/vnd.github+json",
        ],
    )?;
    parse_check(status, &body)
}
fn parse_check(status: u16, body: &[u8]) -> Result<Check> {
    match status {
        200 => {}
        404 => return Ok(Check::Unavailable),
        403 | 429 => bail!("GitHub 请求过于频繁，请稍后再试。"),
        _ => bail!("GitHub 返回 HTTP {status}"),
    }
    let release: Release = serde_json::from_slice(body).context("GitHub 版本信息无效")?;
    ensure!(
        parse_version(release.version()).is_some(),
        "无法识别版本号：{}",
        release.tag_name
    );
    Ok(if newer(release.version(), CURRENT) {
        Check::Available(release)
    } else {
        Check::Current
    })
}

/// Downloads and verifies the release next to the running bundle, so `install` can swap it in.
pub fn stage(release: &Release, proxy: Option<u16>) -> Result<PathBuf> {
    let app = bundle()?;
    let parent = app.parent().unwrap();
    // Only one FlowBar runs at a time, so any other staging directory is left from a failed update.
    for entry in fs::read_dir(parent)?.flatten() {
        if entry.file_name().to_string_lossy().starts_with(STAGING) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    let name = asset_name(release.version());
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == name)
        .with_context(|| format!("该版本没有适用于本机的安装包（{name}）。"))?;
    let staging = parent.join(format!("{STAGING}{}", Uuid::new_v4()));
    #[cfg_attr(windows, allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&staging).with_context(|| {
        format!(
            "无法写入 {}，请把 FlowBar 放在可写的目录中再更新。",
            parent.display()
        )
    })?;
    let result = (|| {
        let archive = staging.join(&name);
        let (status, _) = curl(
            &asset.browser_download_url,
            proxy,
            &[
                "--output",
                archive.to_str().context("应用路径编码无效")?,
                "--max-filesize",
                "268435456",
                "--speed-limit",
                "1024",
                "--speed-time",
                "60",
                "--max-time",
                "1800",
            ],
        )?;
        ensure!(status == 200, "下载安装包失败：HTTP {status}");
        ensure!(
            fs::metadata(&archive)?.len() == asset.size,
            "安装包大小不符，下载可能不完整。"
        );
        if let Some(expected) = asset
            .digest
            .as_deref()
            .and_then(|d| d.strip_prefix("sha256:"))
        {
            ensure!(
                sha256(&archive)?.eq_ignore_ascii_case(expected),
                "安装包校验失败。"
            );
        }
        unpack(&archive, &staging)?;
        fs::remove_file(&archive)?;
        verify(&staging.join(BUNDLE), &app, release.version())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(staging)
}

/// Swaps the staged bundle into place and reopens it once this process exits.
#[cfg(target_os = "macos")]
pub fn install(staging: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let app = bundle()?;
    let new = staging.join(BUNDLE);
    let swapped = (|| {
        let from = CString::new(new.as_os_str().as_bytes())?;
        let to = CString::new(app.as_os_str().as_bytes())?;
        ensure!(
            unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) } == 0,
            "无法替换应用：{}",
            std::io::Error::last_os_error()
        );
        Ok(())
    })();
    if swapped.is_err() {
        let _ = fs::remove_dir_all(staging);
    }
    swapped?;
    let mut relaunch = Command::new("/bin/sh");
    relaunch
        .args(["-c", RELAUNCH, "sh", &std::process::id().to_string()])
        .arg(&app)
        .arg(staging)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    process::spawn_detached(&mut relaunch)
        .context("新版本已安装，但无法自动重新打开，请退出后手动打开 FlowBar")?
        .detach();
    Ok(())
}
/// Exchanges the staged directory with the installed one; the running program keeps its
/// open files, and the old version stays in the staging directory until the relaunch.
#[cfg(target_os = "linux")]
pub fn install(staging: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let app = bundle()?;
    let new = staging.join(BUNDLE);
    let swapped = (|| {
        let from = CString::new(new.as_os_str().as_bytes())?;
        let to = CString::new(app.as_os_str().as_bytes())?;
        let exchanged = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        } == 0;
        if !exchanged {
            // Some file systems cannot exchange; move the old version aside instead.
            let previous = staging.join("previous");
            fs::rename(&app, &previous).context("无法替换应用")?;
            if let Err(error) = fs::rename(&new, &app) {
                let _ = fs::rename(&previous, &app);
                return Err(error).context("无法替换应用");
            }
        }
        Ok(())
    })();
    if swapped.is_err() {
        let _ = fs::remove_dir_all(staging);
    }
    swapped?;
    relaunch(&app, staging)
}
/// A running program's directory cannot be renamed on Windows, but its files can: each file
/// moves aside into the staging directory before the new one takes its place.
#[cfg(windows)]
pub fn install(staging: &Path) -> Result<()> {
    let app = bundle()?;
    let new = staging.join(BUNDLE);
    let previous = staging.join("previous");
    fs::create_dir(&previous)?;
    let mut moved = vec![];
    let mut placed = vec![];
    let swapped = (|| -> Result<()> {
        for entry in fs::read_dir(&new)? {
            let name = entry?.file_name();
            let current = app.join(&name);
            if current.exists() {
                fs::rename(&current, previous.join(&name))?;
                moved.push(name.clone());
            }
            fs::rename(new.join(&name), &current)?;
            placed.push(name);
        }
        Ok(())
    })();
    if let Err(error) = swapped {
        for name in placed.iter().rev() {
            let _ = fs::rename(app.join(name), new.join(name));
        }
        for name in moved.iter().rev() {
            let _ = fs::rename(previous.join(name), app.join(name));
        }
        let _ = fs::remove_dir_all(staging);
        return Err(error).context("无法替换应用");
    }
    relaunch(&app, staging)
}
/// The new version waits for this process to exit, then removes the staging directory.
#[cfg(not(target_os = "macos"))]
fn relaunch(app: &Path, staging: &Path) -> Result<()> {
    let mut command = Command::new(app.join(EXECUTABLE));
    command
        .arg("--updated")
        .arg(std::process::id().to_string())
        .arg(staging)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    process::spawn_detached(&mut command)
        .context("新版本已安装，但无法自动重新打开，请退出后手动打开 FlowBar")?
        .detach();
    Ok(())
}
/// Runs in the relaunched new version before its interface starts.
#[cfg(not(target_os = "macos"))]
pub fn finish(previous: u32, staging: &Path) {
    use std::time::Duration;
    // A second FlowBar would only forward to the old one.
    process::wait_exit(previous, Duration::from_secs(60));
    let ours = staging
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(STAGING));
    if ours {
        // On Windows the old supervisor may still hold its files for a moment.
        for _ in 0..20 {
            if fs::remove_dir_all(staging).is_ok() || !staging.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

/// The running app bundle, which an update replaces in place.
#[cfg(target_os = "macos")]
fn bundle() -> Result<PathBuf> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let app = executable
        .ancestors()
        .nth(3)
        .filter(|app| {
            app.extension().is_some_and(|e| e == "app")
                && executable.parent().and_then(Path::file_name) == Some(OsStr::new("MacOS"))
        })
        .context("只能更新打包后的 FlowBar.app。")?;
    ensure!(
        !app.to_string_lossy().contains("/AppTranslocation/"),
        "请先把 FlowBar 移到「应用程序」文件夹，重新打开后再更新。"
    );
    Ok(app.to_path_buf())
}
/// The unpacked release directory holding the running program and its core.
#[cfg(not(target_os = "macos"))]
fn bundle() -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    // Windows canonical paths carry a \\?\ prefix that other programs mishandle.
    #[cfg(unix)]
    let executable = executable.canonicalize()?;
    let app = executable
        .parent()
        .filter(|app| {
            app.file_name() == Some(OsStr::new(BUNDLE)) && app.join(supervisor::CORE).is_file()
        })
        .context("只能更新解压后的 FlowBar 目录。")?;
    Ok(app.to_path_buf())
}

#[cfg(target_os = "macos")]
fn unpack(archive: &Path, staging: &Path) -> Result<()> {
    run(Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(archive)
        .arg(staging))?;
    Ok(())
}
#[cfg(target_os = "linux")]
fn unpack(archive: &Path, staging: &Path) -> Result<()> {
    let tar = ["/usr/bin/tar", "/bin/tar"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .context("找不到 tar，无法解压安装包。")?;
    run(Command::new(tar)
        .args(["--no-same-owner", "-xzf"])
        .arg(archive)
        .arg("-C")
        .arg(staging))?;
    Ok(())
}
/// Windows 10 and later include bsdtar, which also reads zip archives.
#[cfg(windows)]
fn unpack(archive: &Path, staging: &Path) -> Result<()> {
    run(Command::new(system32("tar.exe")?)
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(staging))?;
    Ok(())
}
#[cfg(windows)]
fn system32(program: &str) -> Result<PathBuf> {
    let root = std::env::var_os("SystemRoot").context("找不到 Windows 系统目录")?;
    Ok(PathBuf::from(root).join("System32").join(program))
}

#[cfg(target_os = "macos")]
fn verify(new: &Path, app: &Path, version: &str) -> Result<()> {
    ensure!(
        fs::symlink_metadata(new).is_ok_and(|m| m.is_dir()),
        "安装包中缺少 FlowBar.app。"
    );
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(new))
    .context("安装包签名校验失败")?;
    let plist = |app: &Path, key: &str| {
        run(Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(app.join("Contents/Info.plist")))
        .map(|s| s.trim().to_owned())
    };
    ensure!(
        plist(new, "CFBundleIdentifier")? == plist(app, "CFBundleIdentifier")?,
        "安装包不是 FlowBar。"
    );
    ensure!(
        plist(new, "CFBundleShortVersionString")? == version,
        "安装包版本与发布版本不符。"
    );
    ensure!(
        new.join("Contents/Resources")
            .join(supervisor::CORE)
            .is_file(),
        "安装包中缺少 mihomo 内核。"
    );
    verify_runs(new, version)
}
#[cfg(not(target_os = "macos"))]
fn verify(new: &Path, _app: &Path, version: &str) -> Result<()> {
    ensure!(
        fs::symlink_metadata(new).is_ok_and(|m| m.is_dir()),
        "安装包中缺少 FlowBar 目录。"
    );
    ensure!(
        new.join(supervisor::CORE).is_file(),
        "安装包中缺少 mihomo 内核。"
    );
    verify_runs(new, version)
}
/// Also proves the new executable runs on this machine.
fn verify_runs(new: &Path, version: &str) -> Result<()> {
    let output = run(Command::new(new.join(EXECUTABLE)).arg("--version"))?;
    ensure!(
        output.split_whitespace().nth(1) == Some(version),
        "新版本无法在本机运行。"
    );
    Ok(())
}

/// Returns the HTTP status and, unless `--output` is given, the response body.
fn curl(url: &str, proxy: Option<u16>, arguments: &[&str]) -> Result<(u16, Vec<u8>)> {
    #[cfg(unix)]
    let program = PathBuf::from("/usr/bin/curl");
    // Windows 10 and later include curl.
    #[cfg(windows)]
    let program = system32("curl.exe")?;
    ensure!(program.is_file(), "找不到 curl，无法连接 GitHub。");
    let mut command = Command::new(program);
    // -q must come first: it skips the user's ~/.curlrc.
    command
        .args([
            "-q",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "15",
            "--user-agent",
            concat!("FlowBar/", env!("CARGO_PKG_VERSION")),
            "--write-out",
            "\n%{http_code}",
        ])
        .args(arguments);
    process::clean_env(&mut command);
    if let Some(port) = proxy {
        command.args(["--proxy", &format!("http://127.0.0.1:{port}")]);
    }
    command.arg(url);
    let output = process::background(&mut command)
        .stdin(Stdio::null())
        .output()
        .context("无法运行 curl")?;
    ensure!(
        output.status.success(),
        "网络请求失败：{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    split_status(output.stdout)
}
fn split_status(mut stdout: Vec<u8>) -> Result<(u16, Vec<u8>)> {
    let split = stdout
        .iter()
        .rposition(|&b| b == b'\n')
        .context("HTTP 状态缺失")?;
    let status = std::str::from_utf8(&stdout[split + 1..])?.trim().parse()?;
    stdout.truncate(split);
    Ok((status, stdout))
}
fn sha256(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
fn run(command: &mut Command) -> Result<String> {
    let output = process::background(command).stdin(Stdio::null()).output()?;
    ensure!(
        output.status.success(),
        "{} 失败：{}",
        command.get_program().to_string_lossy(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn asset_name(version: &str) -> String {
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x86_64"
    };
    let (system, extension) = if cfg!(target_os = "macos") {
        ("macos", "zip")
    } else if cfg!(windows) {
        ("windows", "zip")
    } else {
        ("linux", "tar.gz")
    };
    format!("FlowBar-{version}-{system}-{arch}.{extension}")
}
/// Release versions are numeric; a pre-release suffix compares equal to its release.
fn parse_version(text: &str) -> Option<Vec<u64>> {
    let core = text
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()?;
    let parts = core
        .split('.')
        .map(|part| part.parse().ok())
        .collect::<Option<Vec<u64>>>()?;
    (parts.len() <= 4).then_some(parts)
}
fn newer(candidate: &str, current: &str) -> bool {
    let (Some(mut candidate), Some(mut current)) =
        (parse_version(candidate), parse_version(current))
    else {
        return false;
    };
    let length = candidate.len().max(current.len());
    candidate.resize(length, 0);
    current.resize(length, 0);
    candidate > current
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separates_unavailable_update_sources_from_network_and_release_errors() {
        assert!(matches!(
            parse_check(404, b"{} ").unwrap(),
            Check::Unavailable
        ));
        assert!(parse_check(403, b"{}").is_err());
        assert!(parse_check(429, b"{}").is_err());
        assert!(parse_check(500, b"{}").is_err());
        assert!(parse_check(200, b"not json").is_err());
        let current = format!(r#"{{"tag_name":"v{CURRENT}","html_url":"https://github.com/x/y"}}"#);
        assert!(matches!(
            parse_check(200, current.as_bytes()).unwrap(),
            Check::Current
        ));
        assert!(matches!(
            parse_check(
                200,
                br#"{"tag_name":"v999.0.0","html_url":"https://github.com/x/y"}"#
            )
            .unwrap(),
            Check::Available(_)
        ));
    }
    #[test]
    fn compares_versions() {
        assert!(newer("v0.3.0", "0.2.1"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1", "0.9"));
        assert!(!newer("0.2.1", "0.2.1"));
        assert!(!newer("0.2.1.0", "0.2.1"));
        assert!(!newer("0.2.0", "0.2.1"));
        assert!(!newer("0.3.0-beta.1", "0.3.0"));
        assert!(!newer("latest", "0.2.1"));
        assert!(!newer("0..1", "0.0.0"));
    }
    #[test]
    fn reads_release_and_status() {
        let release: Release = serde_json::from_str(
            r#"{"tag_name":"v0.3.0","html_url":"https://github.com/x/y","body":null,
            "assets":[{"name":"FlowBar-0.3.0-macos-arm64.zip","size":3,
            "browser_download_url":"https://example.com/a.zip","digest":null}]}"#,
        )
        .unwrap();
        assert_eq!(release.version(), "0.3.0");
        assert!(release.body.is_none() && release.assets[0].digest.is_none());
        let name = asset_name("0.3.0");
        assert!(name.starts_with("FlowBar-0.3.0-"));
        #[cfg(target_os = "macos")]
        assert!(name.starts_with("FlowBar-0.3.0-macos-") && name.ends_with(".zip"));
        #[cfg(target_os = "linux")]
        assert!(name.starts_with("FlowBar-0.3.0-linux-") && name.ends_with(".tar.gz"));
        #[cfg(windows)]
        assert!(name.starts_with("FlowBar-0.3.0-windows-") && name.ends_with(".zip"));
        assert_eq!(
            split_status(b"{\"a\":\n1}\n200".to_vec()).unwrap(),
            (200, b"{\"a\":\n1}".to_vec())
        );
        assert_eq!(split_status(b"\n404".to_vec()).unwrap(), (404, vec![]));
        assert!(split_status(b"200".to_vec()).is_err());
    }
    #[test]
    fn hashes_files() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), b"abc").unwrap();
        assert_eq!(
            sha256(file.path()).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn finish_removes_only_staging_directories() {
        let parent = tempfile::tempdir().unwrap();
        let staging = parent.path().join(format!("{STAGING}test"));
        let other = parent.path().join("FlowBar");
        fs::create_dir_all(staging.join("previous")).unwrap();
        fs::create_dir(&other).unwrap();
        // No system assigns this process ID.
        finish(i32::MAX as u32, &staging);
        finish(i32::MAX as u32, &other);
        assert!(!staging.exists() && other.exists());
    }
    #[test]
    fn bundle_version_matches_crate() {
        assert!(include_str!("../Resources/Info.plist").contains(&format!(
            "<key>CFBundleShortVersionString</key><string>{CURRENT}</string>"
        )));
    }
}
