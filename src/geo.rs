//! The Geo databases the core opens while it parses rules, before any node can carry traffic.
//! A missing one comes from the copy bundled with a release, or from the fastest of several
//! mirrors; the configuration check would otherwise fetch it directly from GitHub.
use crate::{config, process, update};
use anyhow::{Result, bail, ensure};
use serde_yaml_ng::Value;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    thread,
    time::Duration,
};
use uuid::Uuid;

/// One database, under the file names the core (like mihomo) accepts for it without regard to case.
pub struct Geo {
    pub label: &'static str,
    /// The first name is the one bundled and downloaded.
    names: &'static [&'static str],
    /// The profile's `geox-url` key, and the file name on the mirrors.
    key: &'static str,
    remote: &'static str,
}
static MMDB: Geo = Geo {
    label: "Country.mmdb / geoip.metadb",
    names: &["geoip.metadb", "Country.mmdb", "geoip.db"],
    key: "mmdb",
    remote: "geoip.metadb",
};
static GEOIP: Geo = Geo {
    label: "GeoIP.dat",
    names: &["GeoIP.dat"],
    key: "geoip",
    remote: "geoip.dat",
};
static GEOSITE: Geo = Geo {
    label: "GeoSite.dat",
    names: &["GeoSite.dat"],
    key: "geosite",
    remote: "geosite.dat",
};
static ASN: Geo = Geo {
    label: "ASN.mmdb",
    names: &["ASN.mmdb"],
    key: "asn",
    remote: "GeoLite2-ASN.mmdb",
};
/// meta-rules-dat publishes the same files at each; the core's own default, as mihomo's, is the first.
const MIRRORS: &[&str] = &[
    "https://github.com/MetaCubeX/meta-rules-dat/releases/download/latest/",
    "https://testingcf.jsdelivr.net/gh/MetaCubeX/meta-rules-dat@release/",
    "https://cdn.jsdelivr.net/gh/MetaCubeX/meta-rules-dat@release/",
    "https://fastly.jsdelivr.net/gh/MetaCubeX/meta-rules-dat@release/",
];
#[cfg(windows)]
const NULL: &str = "NUL";
#[cfg(not(windows))]
const NULL: &str = "/dev/null";

/// The databases that inline rules and DNS need and `data` lacks in a usable form.
/// Providers are loaded by the core itself; its live log remains authoritative.
pub fn missing(yaml: &str, data: &Path) -> Result<Vec<&'static Geo>> {
    fn rule_uses(value: &Value, kinds: &[&str]) -> bool {
        match value {
            Value::String(rule) => rule.split(['(', ')']).any(|part| {
                let mut fields = part.split(',').map(str::trim);
                let kind = fields.next().unwrap_or_default();
                kinds.contains(&kind)
                    && !(kind.ends_with("GEOIP")
                        && fields.next().is_some_and(|s| s.eq_ignore_ascii_case("lan")))
            }),
            Value::Sequence(values) => values.iter().any(|v| rule_uses(v, kinds)),
            Value::Mapping(values) => values.values().any(|v| rule_uses(v, kinds)),
            _ => false,
        }
    }
    fn geosite_dns(value: &Value) -> bool {
        match value {
            Value::String(s) => s.starts_with("geosite:"),
            Value::Sequence(values) => values.iter().any(geosite_dns),
            Value::Mapping(values) => values
                .iter()
                .any(|(key, value)| geosite_dns(key) || geosite_dns(value)),
            _ => false,
        }
    }
    let root = config::parse(yaml)?;
    let uses =
        |kinds: &[&str]| rule_uses(&root["rules"], kinds) || rule_uses(&root["sub-rules"], kinds);
    let dns = &root["dns"];
    let fallback = dns["fallback"].as_sequence().is_some_and(|v| !v.is_empty());
    let filter = &dns["fallback-filter"];
    let ip = uses(&["GEOIP", "SRC-GEOIP"])
        || (fallback
            && filter["geoip"].as_bool() != Some(false)
            && !filter["geoip-code"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("lan")));
    let site = uses(&["GEOSITE"])
        || geosite_dns(dns)
        || (fallback
            && filter["geosite"]
                .as_sequence()
                .is_some_and(|v| !v.is_empty()));
    let dat = root["geodata-mode"].as_bool() == Some(true);
    let candidates = [
        (if dat { &GEOIP } else { &MMDB }, ip),
        (&GEOSITE, site),
        (&ASN, uses(&["IP-ASN", "SRC-IP-ASN"])),
    ];
    let mut missing = vec![];
    for (geo, needed) in candidates {
        if needed && !files(geo, data)?.iter().any(|path| valid(geo, path)) {
            missing.push(geo);
        }
    }
    Ok(missing)
}
/// The regular files in `data` that the core would open for `geo`.
fn files(geo: &Geo, data: &Path) -> Result<Vec<PathBuf>> {
    Ok(fs::read_dir(data)?
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            entry.file_type().is_ok_and(|t| t.is_file())
                && geo
                    .names
                    .iter()
                    .any(|n| name.to_string_lossy().eq_ignore_ascii_case(n))
        })
        .map(|entry| entry.path())
        .collect())
}
/// False for the truncated file an interrupted download leaves. A MaxMind DB ends with its
/// metadata, at most 128 KiB after a marker; a dat file is a protobuf list whose last field
/// ends exactly at the end of the file.
fn valid(geo: &Geo, path: &Path) -> bool {
    if geo.remote.ends_with(".dat") {
        fs::read(path).is_ok_and(|bytes| protobuf(&bytes))
    } else {
        mmdb(path).unwrap_or(false)
    }
}
fn mmdb(path: &Path) -> Result<bool> {
    const MARKER: &[u8] = b"\xAB\xCD\xEFMaxMind.com";
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(128 * 1024)))?;
    let mut tail = vec![];
    file.read_to_end(&mut tail)?;
    Ok(tail.windows(MARKER.len()).any(|window| window == MARKER))
}
fn protobuf(bytes: &[u8]) -> bool {
    fn varint(bytes: &[u8], offset: &mut usize) -> Option<u64> {
        let mut value = 0;
        for shift in (0..64).step_by(7) {
            let byte = *bytes.get(*offset)?;
            *offset += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Some(value);
            }
        }
        None
    }
    let mut offset = 0;
    while offset < bytes.len() {
        let length = match varint(bytes, &mut offset).map(|key| key & 7) {
            Some(0) => varint(bytes, &mut offset).map(|_| 0),
            Some(1) => Some(8),
            Some(2) => varint(bytes, &mut offset),
            Some(5) => Some(4),
            _ => None,
        };
        match length.and_then(|n| offset.checked_add(usize::try_from(n).ok()?)) {
            Some(end) if end <= bytes.len() => offset = end,
            _ => return false,
        }
    }
    !bytes.is_empty()
}

/// For a database `missing` reported: removes its unusable files, which the core would open
/// before a new copy, then copies in the one bundled in `resources` if this build has it.
pub fn install_bundled(geo: &Geo, resources: &Path, data: &Path) -> Result<bool> {
    for path in files(geo, data)? {
        fs::remove_file(path)?;
    }
    let bundled = resources.join(geo.names[0]);
    if !valid(geo, &bundled) {
        return Ok(false);
    }
    config::private_write(&data.join(geo.names[0]), &fs::read(bundled)?)?;
    Ok(true)
}

/// Downloads `geo` into `data`: from the profile's `geox-url` first when it answers, then
/// from the mirrors, fastest first. False when the system has no curl, which leaves the
/// download to the core.
pub fn download(
    geo: &Geo,
    yaml: &str,
    data: &Path,
    stage: &Path,
    cancelled: &AtomicBool,
    pid: &AtomicU32,
    progress: &dyn Fn(String),
) -> Result<bool> {
    if update::curl_command().is_err() {
        return Ok(false);
    }
    let configured = config::parse(yaml)?["geox-url"][geo.key]
        .as_str()
        .map(str::to_owned);
    let mut urls: Vec<String> = configured.iter().cloned().collect();
    for mirror in MIRRORS {
        let url = format!("{mirror}{}", geo.remote);
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    let log = &stage.join("download.log");
    progress(format!("正在为 {} 测试下载源…", geo.label));
    let speeds: Vec<Option<f64>> = thread::scope(|scope| {
        let probes: Vec<_> = urls
            .iter()
            .map(|url| scope.spawn(move || probe(url, log, cancelled)))
            .collect();
        probes
            .into_iter()
            .map(|probe| probe.join().ok().and_then(|speed| speed.ok().flatten()))
            .collect()
    });
    ensure!(!cancelled.load(Ordering::SeqCst), "操作已取消");
    let partial = data.join(format!(".{}.part", geo.names[0]));
    let result = (|| -> Result<()> {
        for index in order(&speeds, configured.is_some()) {
            let url = &urls[index];
            progress(format!(
                "正在下载地理数据库（{}，来自 {}）…",
                geo.label,
                host(url)
            ));
            let mut command = update::curl_command()?;
            command
                .args(["--fail", "--connect-timeout", "10", "--max-time", "300"])
                .args(["--speed-limit", "4096", "--speed-time", "15"])
                .args(["--max-filesize", "67108864", "--output"])
                .arg(&partial);
            let timeout = Duration::from_secs(310);
            if fetch(command, url, "下载", timeout, log, cancelled, pid)?.is_some()
                && valid(geo, &partial)
            {
                #[cfg(unix)]
                fs::set_permissions(&partial, fs::Permissions::from_mode(0o600))?;
                fs::rename(&partial, data.join(geo.names[0]))?;
                return Ok(());
            }
        }
        bail!(
            "地理数据库 {} 下载失败，{} 个下载源都未能完成。\n\n请将 {} 复制到 {} 后重试，\
             或在配置的 geox-url 中指定可访问的地址。各下载源的结果见 download.log。",
            geo.label,
            urls.len(),
            geo.label,
            data.display()
        )
    })();
    let _ = fs::remove_file(&partial);
    result.map(|()| true)
}
/// Bytes per second over the first 256 KiB, or None when the mirror does not answer in time.
fn probe(url: &str, log: &Path, cancelled: &AtomicBool) -> Result<Option<f64>> {
    let mut command = update::curl_command()?;
    command
        .args(["--fail", "--connect-timeout", "5", "--max-time", "8"])
        .args(["--range", "0-262143", "--output", NULL])
        .args(["--write-out", "speed %{speed_download} B/s"]);
    let output = fetch(
        command,
        url,
        "测速",
        Duration::from_secs(10),
        log,
        cancelled,
        &AtomicU32::new(0),
    )?;
    Ok(output
        .as_deref()
        .and_then(|output| output.trim().strip_prefix("speed ")?.strip_suffix(" B/s"))
        .and_then(|speed| speed.parse().ok())
        .filter(|speed| *speed > 0.0))
}
/// Candidate indices by preference: answering ones first, led by the profile's own address
/// (index 0 when `configured`), then by speed. The rest keep their order as a last resort,
/// since a mirror too slow for the probe may still complete a download.
fn order(speeds: &[Option<f64>], configured: bool) -> Vec<usize> {
    let rank = |i: usize| (speeds[i].is_some(), configured && i == 0);
    let mut order: Vec<usize> = (0..speeds.len()).collect();
    order.sort_by(|&a, &b| {
        rank(b).cmp(&rank(a)).then(
            speeds[b]
                .unwrap_or(0.0)
                .total_cmp(&speeds[a].unwrap_or(0.0)),
        )
    });
    order
}
/// Runs curl on `url` and appends the outcome to `log`. Returns its output when the
/// transfer succeeds; only cancellation is an error.
fn fetch(
    mut command: Command,
    url: &str,
    label: &str,
    timeout: Duration,
    log: &Path,
    cancelled: &AtomicBool,
    pid: &AtomicU32,
) -> Result<Option<String>> {
    command.arg(url);
    let path = log.with_file_name(format!("curl-{}.log", Uuid::new_v4()));
    let result = process::command_logged(command, timeout, cancelled, pid, &path, |_| {});
    let output = fs::read_to_string(&path).unwrap_or_default();
    let _ = fs::remove_file(&path);
    ensure!(!cancelled.load(Ordering::SeqCst), "操作已取消");
    let outcome = match &result {
        Ok(result) if result.status.success() => "成功".into(),
        Ok(result) => format!("失败（{}）", result.status),
        Err(error) => format!("失败（{error}）"),
    };
    let mut entry = format!("[{label}] {url}：{outcome}\n{output}");
    if !entry.ends_with('\n') {
        entry.push('\n');
    }
    crate::logging::record(log, entry);
    Ok(result
        .is_ok_and(|result| result.status.success())
        .then_some(output))
}
fn host(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The smallest files that pass `valid`: a MaxMind DB marker and an empty protobuf field.
    const MMDB_FILE: &[u8] = b"\xAB\xCD\xEFMaxMind.com";
    const DAT_FILE: &[u8] = &[0x0A, 0x00];

    #[test]
    fn preflights_geo_rules_modes_aliases_and_dns() {
        let data = tempfile::tempdir().unwrap();
        let missing = |yaml: &str| -> Vec<_> {
            super::missing(yaml, data.path())
                .unwrap()
                .iter()
                .map(|geo| geo.label)
                .collect()
        };
        let yaml =
            "proxies: []\nrules: ['GEOIP,CN,DIRECT', 'GEOSITE,cn,DIRECT', 'IP-ASN,123,DIRECT']\n";
        assert_eq!(
            missing(yaml),
            ["Country.mmdb / geoip.metadb", "GeoSite.dat", "ASN.mmdb"]
        );
        // The core accepts these names without regard to case, including legacy MMDB aliases.
        fs::write(data.path().join("geoip.db"), MMDB_FILE).unwrap();
        fs::write(data.path().join("geosite.dat"), DAT_FILE).unwrap();
        fs::write(data.path().join("asn.mmdb"), MMDB_FILE).unwrap();
        assert!(missing(yaml).is_empty());
        assert_eq!(
            missing(&format!("{yaml}geodata-mode: true\n")),
            ["GeoIP.dat"]
        );
        fs::write(data.path().join("geoip.dat"), []).unwrap();
        assert_eq!(
            missing(&format!("{yaml}geodata-mode: true\n")),
            ["GeoIP.dat"]
        );
        fs::write(data.path().join("geoip.dat"), DAT_FILE).unwrap();
        assert!(missing(&format!("{yaml}geodata-mode: true\n")).is_empty());
        // An interrupted download leaves a file that the core would only replace by downloading.
        fs::write(data.path().join("geosite.dat"), [0x0A, 0x05, 0x01]).unwrap();
        assert_eq!(missing(yaml), ["GeoSite.dat"]);

        let empty = tempfile::tempdir().unwrap();
        let check = |yaml: &str| -> Vec<_> {
            super::missing(yaml, empty.path())
                .unwrap()
                .iter()
                .map(|geo| geo.label)
                .collect()
        };
        assert!(check("proxies: []\nrules: ['GEOIP,LAN,DIRECT', 'MATCH,DIRECT']").is_empty());
        assert_eq!(
            check("proxies: []\nsub-rules:\n  cn: ['AND,((NETWORK,TCP),(SRC-GEOIP,CN)),DIRECT']"),
            ["Country.mmdb / geoip.metadb"]
        );
        assert_eq!(
            check("proxies: []\ndns:\n  nameserver-policy: {'geosite:cn': '223.5.5.5'}"),
            ["GeoSite.dat"]
        );
        assert_eq!(
            check(
                "proxies: []\ndns:\n  fallback: [1.1.1.1]\n  fallback-filter:\n    geosite: [cn]"
            ),
            ["Country.mmdb / geoip.metadb", "GeoSite.dat"]
        );
        assert!(
            check("proxies: []\ndns:\n  fallback: [1.1.1.1]\n  fallback-filter: {geoip: false}")
                .is_empty()
        );
        // Node credentials and names never imply a database dependency.
        assert!(check("proxies: [{name: 'GEOSITE,cn,DIRECT', password: 'geosite:cn'}]").is_empty());
    }
    #[test]
    fn rejects_truncated_databases() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("database");
        let mut mmdb = vec![0; 200 * 1024];
        mmdb.extend_from_slice(MMDB_FILE);
        mmdb.extend_from_slice(b"metadata");
        fs::write(&path, &mmdb).unwrap();
        assert!(valid(&MMDB, &path));
        fs::write(&path, &mmdb[..200 * 1024]).unwrap();
        assert!(!valid(&MMDB, &path));
        // A marker more than 128 KiB before the end is part of the data, not the metadata.
        mmdb.extend_from_slice(&[0; 129 * 1024]);
        fs::write(&path, &mmdb).unwrap();
        assert!(!valid(&MMDB, &path));

        // Two list entries, a varint and a fixed 32-bit field.
        let dat = [
            0x0A, 0x02, 0x08, 0x01, 0x0A, 0x00, 0x10, 0x96, 0x01, 0x1D, 1, 2, 3, 4,
        ];
        fs::write(&path, dat).unwrap();
        assert!(valid(&GEOSITE, &path));
        for end in 1..dat.len() {
            if ![4, 6, 9].contains(&end) {
                fs::write(&path, &dat[..end]).unwrap();
                assert!(!valid(&GEOSITE, &path), "{end}");
            }
        }
        fs::write(&path, []).unwrap();
        assert!(!valid(&GEOSITE, &path));
        // Groups are not used by these files; a length beyond the file is a truncation.
        fs::write(&path, [0x0B]).unwrap();
        assert!(!valid(&GEOSITE, &path));
        fs::write(&path, [0x0A, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F]).unwrap();
        assert!(!valid(&GEOSITE, &path));
    }
    #[test]
    fn replaces_unusable_copies_with_the_bundled_database() {
        let resources = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        fs::write(data.path().join("Country.mmdb"), b"partial").unwrap();
        fs::write(data.path().join(".geoip.metadb.part"), b"partial").unwrap();
        // Without a bundled copy the file is removed for the download.
        assert!(!install_bundled(&MMDB, resources.path(), data.path()).unwrap());
        assert!(!data.path().join("Country.mmdb").exists());

        fs::write(data.path().join("Country.mmdb"), b"partial").unwrap();
        fs::write(resources.path().join("geoip.metadb"), MMDB_FILE).unwrap();
        assert!(install_bundled(&MMDB, resources.path(), data.path()).unwrap());
        assert!(!data.path().join("Country.mmdb").exists());
        assert_eq!(
            fs::read(data.path().join("geoip.metadb")).unwrap(),
            MMDB_FILE
        );
        assert!(data.path().join(".geoip.metadb.part").exists());
        assert!(
            super::missing("proxies: []\nrules: [GEOIP,CN,DIRECT]", data.path())
                .unwrap()
                .is_empty()
        );

        // A damaged bundle is not copied.
        fs::write(resources.path().join("GeoSite.dat"), [0x0A, 0x05]).unwrap();
        assert!(!install_bundled(&GEOSITE, resources.path(), data.path()).unwrap());
        assert!(!data.path().join("GeoSite.dat").exists());
    }
    #[test]
    fn prefers_answering_mirrors_by_speed_after_the_profile_address() {
        let speeds = [Some(10.0), None, Some(30.0), Some(20.0), None];
        assert_eq!(order(&speeds, true), [0, 2, 3, 1, 4]);
        assert_eq!(order(&speeds, false), [2, 3, 0, 1, 4]);
        // A configured address that does not answer is still the first fallback.
        let speeds = [None, Some(1.0), None];
        assert_eq!(order(&speeds, true), [1, 0, 2]);
        assert_eq!(
            host(&format!("{}geoip.metadb", MIRRORS[1])),
            "testingcf.jsdelivr.net"
        );
    }
}
