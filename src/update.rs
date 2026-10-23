use crate::process;
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    ffi::{CString, OsStr},
    fs,
    os::unix::{ffi::OsStrExt, fs::DirBuilderExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use uuid::Uuid;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const STAGING: &str = ".FlowBar-update-";
/// Waits for the old process to exit, because a second FlowBar only forwards to the first,
/// then removes the previous version that the swap left in the staging directory.
const RELAUNCH: &str = r#"i=0; while kill -0 "$1" 2>/dev/null && [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done; rm -rf "$3"; exec /usr/bin/open "$2""#;

#[derive(Debug)]
pub enum Event {
    Due,
    Checked(bool, Result<Option<Release>, String>),
    Staged(Result<PathBuf, String>),
}
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
pub fn check(proxy: Option<u16>) -> Result<Option<Release>> {
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
    match status {
        200 => {}
        404 => bail!("GitHub 上还没有正式发布的版本，或仓库不是公开的。"),
        403 | 429 => bail!("GitHub 请求过于频繁，请稍后再试。"),
        _ => bail!("GitHub 返回 HTTP {status}"),
    }
    let release: Release = serde_json::from_slice(&body).context("GitHub 版本信息无效")?;
    ensure!(
        parse_version(release.version()).is_some(),
        "无法识别版本号：{}",
        release.tag_name
    );
    Ok(newer(release.version(), CURRENT).then_some(release))
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
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .with_context(|| {
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
            let output = run(Command::new("/usr/bin/shasum")
                .args(["-a", "256"])
                .arg(&archive))?;
            ensure!(
                output
                    .split_whitespace()
                    .next()
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(expected)),
                "安装包校验失败。"
            );
        }
        run(Command::new("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(&archive)
            .arg(&staging))?;
        fs::remove_file(&archive)?;
        verify(&staging.join("FlowBar.app"), &app, release.version())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(staging)
}

/// Swaps the staged bundle into place and reopens it once this process exits.
pub fn install(staging: &Path) -> Result<()> {
    let app = bundle()?;
    let new = staging.join("FlowBar.app");
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

/// The running app bundle, which an update replaces in place.
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
        new.join("Contents/Resources/mihomo").is_file(),
        "安装包中缺少 mihomo 内核。"
    );
    // Also proves the new executable runs on this Mac.
    let output = run(Command::new(new.join("Contents/MacOS/FlowBar")).arg("--version"))?;
    ensure!(
        output.split_whitespace().nth(1) == Some(version),
        "新版本无法在本机运行。"
    );
    Ok(())
}

/// Returns the HTTP status and, unless `--output` is given, the response body.
fn curl(url: &str, proxy: Option<u16>, arguments: &[&str]) -> Result<(u16, Vec<u8>)> {
    let mut command = Command::new("/usr/bin/curl");
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
        .args(arguments)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    if let Some(port) = proxy {
        command.args(["--proxy", &format!("http://127.0.0.1:{port}")]);
    }
    command.arg(url);
    let output = command
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
fn run(command: &mut Command) -> Result<String> {
    let output = command.stdin(Stdio::null()).output()?;
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
    format!("FlowBar-{version}-macos-{arch}.zip")
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
        assert!(asset_name("0.3.0").starts_with("FlowBar-0.3.0-macos-"));
        assert_eq!(
            split_status(b"{\"a\":\n1}\n200".to_vec()).unwrap(),
            (200, b"{\"a\":\n1}".to_vec())
        );
        assert_eq!(split_status(b"\n404".to_vec()).unwrap(), (404, vec![]));
        assert!(split_status(b"200".to_vec()).is_err());
    }
    #[test]
    fn bundle_version_matches_crate() {
        assert!(include_str!("../Resources/Info.plist").contains(&format!(
            "<key>CFBundleShortVersionString</key><string>{CURRENT}</string>"
        )));
    }
}
