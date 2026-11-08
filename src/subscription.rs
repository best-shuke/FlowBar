//! Profiles kept up to date from a subscription link: an HTTPS URL that serves a complete
//! mihomo YAML profile. FlowBar downloads it when imported, on request and on the interval
//! the server suggests, and replaces the saved copy only with a profile that parses.

use crate::{config, process, update};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Hours between automatic updates when the server suggests none.
pub const DEFAULT_INTERVAL: u32 = 24;
/// Bounds for a server's suggestion, so a typo cannot poll every minute or never.
const INTERVAL_RANGE: std::ops::RangeInclusive<u32> = 1..=24 * 7;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_URL: usize = 4096;
/// After a failed automatic update, wait this long before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(3600);

/// A profile's subscription, saved in the settings next to the node choices.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Subscription {
    pub url: String,
    /// Hours between automatic updates.
    pub interval: u32,
    /// Seconds since the Unix epoch of the last successful check, which may have found
    /// the profile unchanged.
    pub updated: u64,
    /// The server's tag for the saved copy, for a conditional request.
    pub etag: Option<String>,
    /// The last automatic update's error and when it happened; cleared by a success.
    pub error: Option<String>,
    pub failed: u64,
}
impl Subscription {
    pub fn new(url: String) -> Self {
        Self {
            url,
            interval: DEFAULT_INTERVAL,
            ..Self::default()
        }
    }
    /// Whether an automatic update should run now.
    pub fn due(&self, now: u64) -> bool {
        let interval = u64::from(
            self.interval
                .clamp(*INTERVAL_RANGE.start(), *INTERVAL_RANGE.end()),
        );
        // A time after now was recorded while the clock ran ahead; it would hold back
        // updates until the clock caught up.
        let past = |time: u64| if time > now { 0 } else { time };
        now >= past(self.updated).saturating_add(interval * 3600)
            && now >= past(self.failed).saturating_add(RETRY_AFTER.as_secs())
    }
    /// Records a check that reached the server.
    pub fn checked(&mut self, download: &Download, now: u64) {
        self.updated = now;
        self.error = None;
        self.failed = 0;
        if let Some(hours) = download.interval {
            self.interval = hours;
        }
        if download.yaml.is_some() || download.etag.is_some() {
            self.etag = download.etag.clone();
        }
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// "3 小时前" for the menu; the standard library has no local time zone.
pub fn ago(then: u64, now: u64) -> String {
    let seconds = now.saturating_sub(then);
    match seconds {
        0..60 => "刚刚".into(),
        60..3600 => format!("{} 分钟前", seconds / 60),
        3600..86400 => format!("{} 小时前", seconds / 3600),
        _ => format!("{} 天前", seconds / 86400),
    }
}

/// Trims a pasted link and checks that it is a plain HTTPS URL.
pub fn normalize(url: &str) -> Result<String> {
    let url = url.trim();
    ensure!(!url.is_empty(), "请输入订阅链接。");
    ensure!(url.len() <= MAX_URL, "订阅链接过长。");
    ensure!(
        !url.chars().any(|c| c.is_whitespace() || c.is_control()),
        "订阅链接不能包含空格或换行。"
    );
    let Some(rest) = url
        .get(..8)
        .filter(|scheme| scheme.eq_ignore_ascii_case("https://"))
        .map(|_| &url[8..])
    else {
        bail!("订阅链接须以 https:// 开头，FlowBar 不会用未加密的连接下载配置。");
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.rsplit('@').next().unwrap_or_default();
    ensure!(
        !host.is_empty() && !host.starts_with(':'),
        "订阅链接缺少域名。"
    );
    Ok(format!("https://{rest}"))
}

/// The link without its path and query, which often hold the subscription's secret.
pub fn redacted(url: &str) -> String {
    let rest = url.strip_prefix("https://").unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.rsplit('@').next().unwrap_or_default();
    if rest.len() > host.len() {
        format!("https://{host}/…")
    } else {
        format!("https://{host}")
    }
}

/// A finished request: a new profile, or `yaml: None` when the server's copy is unchanged.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Download {
    pub yaml: Option<String>,
    /// The profile name the server suggests, from its Content-Disposition file name.
    pub name: Option<String>,
    pub interval: Option<u32>,
    pub etag: Option<String>,
}

/// Downloads the profile. Requests go out directly; when that fails to connect and a local
/// proxy runs, they try once more through it, in case the server is only reachable there.
pub fn fetch(url: &str, etag: Option<&str>, proxy: Option<u16>) -> Result<Download> {
    let result = match request(url, etag, None) {
        Err(error) if proxy.is_some() => {
            request(url, etag, proxy).with_context(|| format!("直接连接失败：{error:#}"))
        }
        result => result,
    };
    // curl and the server may echo the request; keep the secret out of alerts and logs.
    result.map_err(|error| anyhow::anyhow!(format!("{error:#}").replace(url, &redacted(url))))
}

/// A finished download, sent to the UI thread.
#[derive(Debug)]
pub enum Event {
    /// The first download of a link the user entered.
    Imported(String, Result<Download, String>),
    /// An update of the named profile from its link; manual updates report their outcome.
    Updated {
        name: String,
        url: String,
        manual: bool,
        result: Result<Download, String>,
    },
}
fn request(url: &str, etag: Option<&str>, proxy: Option<u16>) -> Result<Download> {
    let mut command = update::curl_command()?;
    command.args([
        "--connect-timeout",
        "15",
        "--max-time",
        "90",
        "--max-filesize",
        &MAX_BYTES.to_string(),
        "--max-redirs",
        "5",
        // Headers of every response, redirects included, then the body.
        "--dump-header",
        "-",
        // Many subscription services serve a mihomo profile only to Clash-family clients.
        "--user-agent",
        concat!("FlowBar/", env!("CARGO_PKG_VERSION"), " clash.meta mihomo"),
    ]);
    if let Some(etag) = etag.filter(|etag| valid_header(etag)) {
        command.args(["--header", &format!("If-None-Match: {etag}")]);
    }
    if let Some(port) = proxy {
        command.args(["--proxy", &format!("http://127.0.0.1:{port}")]);
    }
    command.arg("--").arg(url);
    let output = process::background(&mut command)
        .stdin(Stdio::null())
        .output()
        .context("无法运行 curl")?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        let message = message.trim().trim_start_matches("curl: ");
        if output.status.code() == Some(63) {
            bail!("订阅内容超过 {} MB。", MAX_BYTES / 1024 / 1024);
        }
        bail!("网络请求失败：{message}");
    }
    parse_response(&output.stdout)
}
fn valid_header(value: &str) -> bool {
    value.len() <= 256 && value.chars().all(|c| c.is_ascii_graphic() || c == ' ')
}

/// Splits curl's `--dump-header -` output into the final response's status, headers and body.
fn parse_response(output: &[u8]) -> Result<Download> {
    let mut rest = output;
    let mut status = None;
    let mut headers = vec![];
    // Each response of a redirect chain starts with its own status line.
    while rest.starts_with(b"HTTP/") {
        let end = find(rest, b"\r\n\r\n")
            .map(|i| (i, 4))
            .or_else(|| find(rest, b"\n\n").map(|i| (i, 2)))
            .context("服务器响应不完整")?;
        let block = String::from_utf8_lossy(&rest[..end.0]);
        let mut lines = block.lines();
        status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok());
        headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        rest = &rest[end.0 + end.1..];
    }
    let status = status.context("服务器响应无效")?;
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let interval = header("profile-update-interval")
        .and_then(|hours| hours.trim().parse::<f64>().ok())
        .filter(|hours| hours.is_finite() && *hours > 0.0)
        .map(|hours| (hours.round() as u32).clamp(*INTERVAL_RANGE.start(), *INTERVAL_RANGE.end()));
    let etag = header("etag")
        .filter(|etag| valid_header(etag))
        .map(str::to_owned);
    match status {
        200 => {}
        304 => {
            return Ok(Download {
                yaml: None,
                name: None,
                interval,
                etag,
            });
        }
        401 | 403 | 404 | 410 => bail!(
            "服务器拒绝了这个订阅链接（HTTP {status}）。链接可能已失效或被重置，请到订阅网站复制新的链接。"
        ),
        429 => bail!("请求过于频繁（HTTP 429），请稍后再试。"),
        _ => bail!("服务器返回 HTTP {status}。"),
    }
    ensure!(
        rest.len() <= MAX_BYTES,
        "订阅内容超过 {} MB。",
        MAX_BYTES / 1024 / 1024
    );
    let yaml = String::from_utf8(rest.to_vec()).context("订阅内容不是 UTF-8 文本")?;
    let yaml = yaml.strip_prefix('\u{feff}').unwrap_or(&yaml).to_owned();
    if let Err(error) = config::summary(&yaml) {
        bail!(
            "订阅内容不是可用的 mihomo 配置：{error:#}。FlowBar 只支持返回完整 YAML 的订阅，不支持 Base64 节点列表。"
        );
    }
    Ok(Download {
        yaml: Some(yaml),
        name: header("content-disposition").and_then(disposition_name),
        interval,
        etag,
    })
}
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The file name in a Content-Disposition header, without its YAML extension. The RFC 5987
/// `filename*` form wins over the plain one, as browsers do.
fn disposition_name(value: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for part in value.split(';').skip(1) {
        let Some((key, raw)) = part.split_once('=') else {
            continue;
        };
        let raw = raw.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                let mut pieces = raw.splitn(3, '\'');
                let charset = pieces.next().unwrap_or_default();
                let encoded = pieces.nth(1).unwrap_or_default();
                if charset.eq_ignore_ascii_case("utf-8") {
                    extended = percent_decode(encoded);
                }
            }
            "filename" => plain = Some(raw.trim_matches('"').to_owned()),
            _ => {}
        }
    }
    let name = extended.or(plain)?;
    // Some servers send a path; only its last part names the file.
    let name = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    let stem = [".yaml", ".yml"]
        .iter()
        .find(|ext| lower.ends_with(*ext))
        .map_or(name, |ext| &name[..name.len() - ext.len()]);
    let stem = stem.trim();
    (!stem.is_empty() && !stem.starts_with('.')).then(|| stem.to_owned())
}
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// The profile name for a new subscription: the server's file name, or else the host.
pub fn suggested_name(download: &Download, url: &str) -> String {
    let host = || {
        let rest = url.strip_prefix("https://").unwrap_or(url);
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host = host.rsplit('@').next().unwrap_or_default();
        host.split(':').next().unwrap_or_default().to_owned()
    };
    config::profile_name(&download.name.clone().unwrap_or_else(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = "proxies: []\nrules: []\n";

    #[test]
    fn normalizes_only_https_links() {
        assert_eq!(
            normalize("  https://sub.example.com/sub/abc?x=1 \n").unwrap(),
            "https://sub.example.com/sub/abc?x=1"
        );
        assert_eq!(
            normalize("HTTPS://Example.com/a").unwrap(),
            "https://Example.com/a"
        );
        for bad in [
            "",
            "http://example.com/a",
            "ftp://example.com",
            "https://",
            "https:///path",
            "https://example.com/a b",
            "example.com/sub",
        ] {
            assert!(normalize(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn redacts_path_and_credentials() {
        assert_eq!(
            redacted("https://sub.example.com/sub/SECRET"),
            "https://sub.example.com/…"
        );
        assert_eq!(
            redacted("https://user:pw@sub.example.com?token=x"),
            "https://sub.example.com/…"
        );
        assert_eq!(redacted("https://example.com"), "https://example.com");
    }

    #[test]
    fn parses_final_response_after_redirects() {
        let output = format!(
            "HTTP/1.1 302 Found\r\nLocation: https://b.example/x\r\n\r\n\
             HTTP/2 200\r\ncontent-type: text/yaml\r\nETag: \"abc\"\r\n\
             Content-Disposition: attachment; filename=\"plain.yaml\"; filename*=UTF-8''%E6%9C%BA%E5%9C%BA.yaml\r\n\
             profile-update-interval: 12\r\n\r\n{PROFILE}"
        );
        let download = parse_response(output.as_bytes()).unwrap();
        assert_eq!(download.yaml.as_deref(), Some(PROFILE));
        assert_eq!(download.name.as_deref(), Some("机场"));
        assert_eq!(download.interval, Some(12));
        assert_eq!(download.etag.as_deref(), Some("\"abc\""));
    }

    #[test]
    fn reports_unchanged_and_rejected_links() {
        let unchanged = parse_response(b"HTTP/1.1 304 Not Modified\r\nETag: \"abc\"\r\n\r\n");
        assert_eq!(unchanged.unwrap().yaml, None);
        let gone = parse_response(b"HTTP/1.1 404 Not Found\r\n\r\nnot found\n").unwrap_err();
        assert!(gone.to_string().contains("HTTP 404"));
        let base64 = parse_response(b"HTTP/1.1 200 OK\r\n\r\ndm1lc3M6Ly9leGFtcGxl").unwrap_err();
        assert!(base64.to_string().contains("Base64"));
        let html = parse_response(b"HTTP/1.1 200 OK\r\n\r\n<html><body>login</body></html>");
        assert!(html.is_err());
    }

    #[test]
    fn clamps_server_intervals() {
        let parse = |value: &str| {
            let output =
                format!("HTTP/1.1 200 OK\r\nprofile-update-interval: {value}\r\n\r\n{PROFILE}");
            parse_response(output.as_bytes()).unwrap().interval
        };
        assert_eq!(parse("0.2"), Some(1));
        assert_eq!(parse("100000"), Some(168));
        assert_eq!(parse("abc"), None);
        assert_eq!(parse("-3"), None);
    }

    #[test]
    fn reads_disposition_names() {
        assert_eq!(
            disposition_name("attachment; filename=\"zgo-or.yaml\"").as_deref(),
            Some("zgo-or")
        );
        assert_eq!(
            disposition_name("attachment; filename=../../etc/passwd.yml").as_deref(),
            Some("passwd")
        );
        assert_eq!(disposition_name("attachment; filename=\".yaml\""), None);
        assert_eq!(disposition_name("inline"), None);
        let download = Download::default();
        assert_eq!(
            suggested_name(&download, "https://user@sub.example.com:8443/x"),
            "sub.example.com"
        );
    }

    #[test]
    fn schedules_updates_and_retries() {
        let mut subscription = Subscription::new("https://example.com/s".into());
        assert!(subscription.due(1_000_000));
        subscription.updated = 1_000_000;
        assert!(!subscription.due(1_000_000 + 23 * 3600));
        assert!(subscription.due(1_000_000 + 24 * 3600));
        subscription.failed = 1_000_000 + 24 * 3600;
        assert!(!subscription.due(1_000_000 + 24 * 3600 + 60));
        assert!(subscription.due(1_000_000 + 25 * 3600));
        let download = Download {
            interval: Some(6),
            etag: Some("\"x\"".into()),
            ..Download::default()
        };
        subscription.checked(&download, 2_000_000);
        assert_eq!(
            (
                subscription.interval,
                subscription.failed,
                subscription.updated
            ),
            (6, 0, 2_000_000)
        );
        assert_eq!(ago(100, 100 + 7200), "2 小时前");
        // Times recorded while the clock ran ahead do not hold back updates.
        subscription.updated = 9_000_000;
        assert!(subscription.due(2_000_000 + 60));
        subscription.updated = 2_000_000;
        subscription.failed = 9_000_000;
        assert!(subscription.due(2_000_000 + 6 * 3600));
    }

    /// Fetches a real subscription: FLOWBAR_TEST_SUBSCRIPTION=https://… cargo test -- --ignored
    #[test]
    #[ignore]
    fn fetches_live_subscription() {
        let url =
            std::env::var("FLOWBAR_TEST_SUBSCRIPTION").expect("set FLOWBAR_TEST_SUBSCRIPTION");
        let download = fetch(&normalize(&url).unwrap(), None, None).unwrap();
        let yaml = download.yaml.clone().unwrap();
        let summary = config::summary(&yaml).unwrap();
        println!(
            "name={:?} interval={:?} etag={:?} proxies={} groups={} rules={}",
            download.name,
            download.interval,
            download.etag,
            summary.proxies,
            summary.groups.len(),
            summary.rules
        );
        let again = fetch(&url, download.etag.as_deref(), None).unwrap();
        assert_eq!(again.yaml, None, "an unchanged profile should return 304");
    }
}
