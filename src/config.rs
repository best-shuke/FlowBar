use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

/// The per-user data directory below the home directory; root supervisors use it to find
/// the user's copy.
#[cfg(target_os = "macos")]
pub const DATA_DIR: &str = "Library/Application Support/FlowBar";
#[cfg(target_os = "linux")]
pub const DATA_DIR: &str = ".local/share/flowbar";
#[cfg(target_os = "macos")]
const TUN_DEVICE: &str = "utun";
#[cfg(not(target_os = "macos"))]
const TUN_DEVICE: &str = "FlowBar";

pub const GEO_NAMES: &[&str] = &[
    "geoip.dat",
    "geosite.dat",
    "Country.mmdb",
    "ASN.mmdb",
    "GeoIP.dat",
    "GeoSite.dat",
    "geoip.metadb",
    "geoip.db",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// The active profile, one of [`profiles`] once a profile is imported.
    pub profile_name: String,
    pub tun: bool,
    pub ipv6: bool,
    pub proxy_port: u16,
    pub controller_port: u16,
    pub dns_port: u16,
    pub mode: String,
    pub auto_connect: bool,
    pub auto_update: bool,
    /// Node choices of the active profile.
    pub selections: BTreeMap<String, String>,
    /// Node choices of the other profiles, restored when one becomes active again.
    pub profile_selections: BTreeMap<String, BTreeMap<String, String>>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            profile_name: "未导入配置".into(),
            tun: true,
            ipv6: false,
            proxy_port: 17890,
            controller_port: 19090,
            dns_port: 15353,
            mode: "rule".into(),
            auto_connect: false,
            auto_update: true,
            selections: BTreeMap::new(),
            profile_selections: BTreeMap::new(),
        }
    }
}
impl Settings {
    pub fn load() -> Self {
        let mut settings = fs::read(home().join("settings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        migrate_profile(&mut settings);
        settings
    }
    pub fn save(&self) -> Result<()> {
        self.validate()?;
        private_dir(&home())?;
        private_write(
            &home().join("settings.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }
    pub fn validate(&self) -> Result<()> {
        let ports = [self.proxy_port, self.controller_port, self.dns_port];
        ensure!(
            ports.iter().all(|p| *p >= 1024)
                && ports[0] != ports[1]
                && ports[0] != ports[2]
                && ports[1] != ports[2],
            "代理、控制和 DNS 端口须各不相同，且在 1024–65535 之间。"
        );
        ensure!(
            ["rule", "global", "direct"].contains(&self.mode.as_str()),
            "分流模式无效。"
        );
        Ok(())
    }
    /// Makes `name` the active profile; each profile keeps its own node choices.
    pub fn switch_profile(&mut self, name: &str) {
        if self.profile_name == name {
            return;
        }
        let selections = self.profile_selections.remove(name).unwrap_or_default();
        let previous = std::mem::replace(&mut self.selections, selections);
        let previous_name = std::mem::replace(&mut self.profile_name, name.into());
        if !previous.is_empty() {
            self.profile_selections.insert(previous_name, previous);
        }
    }
    /// Deletes the active profile's copy and activates the first remaining profile.
    pub fn remove_profile(&mut self) -> Result<()> {
        match fs::remove_file(profile_path(&self.profile_name)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).context("无法删除配置文件");
            }
            _ => {}
        }
        self.selections.clear();
        let next = profiles()
            .into_iter()
            .next()
            .unwrap_or_else(|| Settings::default().profile_name);
        self.switch_profile(&next);
        self.save()
    }
}

/// Imported profiles, one YAML copy each, named after the profile.
pub fn profiles_dir() -> PathBuf {
    home().join("Profiles")
}
pub fn profile_path(name: &str) -> PathBuf {
    profiles_dir().join(format!("{name}.yaml"))
}
/// The imported profiles' names, sorted.
pub fn profiles() -> Vec<String> {
    list_profiles(&profiles_dir())
}
fn list_profiles(directory: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            Some(name.strip_suffix(".yaml")?.to_owned())
        })
        .filter(|name| !name.is_empty())
        .collect();
    names.sort();
    names
}
/// A profile name that is also a file name on every system.
pub fn profile_name(name: &str) -> String {
    let name: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || r#"/\:*?"<>|"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    if name.is_empty() {
        "配置".into()
    } else {
        name
    }
}
/// FlowBar 0.5 and earlier kept a single imported profile.yaml; it becomes the active
/// profile in the profiles directory.
fn migrate_profile(settings: &mut Settings) {
    let legacy = home().join("profile.yaml");
    if !legacy.is_file() {
        return;
    }
    let name = profile_name(&settings.profile_name);
    if private_dir(&profiles_dir()).is_ok()
        && fs::rename(&legacy, profile_path(&name)).is_ok()
        && name != settings.profile_name
    {
        settings.profile_name = name;
        let _ = settings.save();
    }
}

#[cfg(unix)]
pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is required")).join(DATA_DIR)
}
#[cfg(windows)]
pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA is required"))
        .join("FlowBar")
}
/// Clash Verge Rev keeps its Geo data here; an import from it may reuse them.
fn verge() -> Option<PathBuf> {
    const ID: &str = "io.github.clash-verge-rev.clash-verge-rev";
    #[cfg(unix)]
    return home().parent().map(|parent| parent.join(ID));
    #[cfg(windows)]
    std::env::var_os("APPDATA").map(|roaming| PathBuf::from(roaming).join(ID))
}
pub fn private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    // The per-user application data directory is already private on Windows.
    #[cfg(windows)]
    fs::create_dir_all(path)?;
    ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "数据目录不能是符号链接。"
    );
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write(path, bytes, 0o600)
}
#[cfg_attr(windows, allow(unused_variables))]
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let temporary = path.with_file_name(format!(".flowbar-{}", Uuid::new_v4()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(mode);
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// Runs as a session starts; `active` protects live sessions, including the new one.
/// Ended sessions lose any runtime config left by a quit or crash, and only the newest
/// four are kept, so at most five remain once the new session ends.
pub fn prune_sessions(sessions: &Path, active: impl Fn(&str) -> bool) {
    const KEEP: usize = 4;
    let Ok(entries) = fs::read_dir(sessions) else {
        return;
    };
    let mut ended: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let id = entry.file_name().into_string().ok()?;
            let metadata = entry.metadata().ok()?;
            (metadata.is_dir() && Uuid::parse_str(&id).is_ok() && !active(&id))
                .then(|| (metadata.modified().ok(), entry.path()))
        })
        .collect();
    ended.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (index, (_, path)) in ended.into_iter().enumerate() {
        if index < KEEP {
            let _ = fs::remove_file(path.join("config.yaml"));
        } else {
            let _ = fs::remove_dir_all(path);
        }
    }
}

pub fn parse(yaml: &str) -> Result<Value> {
    let mut root: Value = serde_yaml_ng::from_str(yaml).context("YAML 语法错误")?;
    root.apply_merge().context("YAML 锚点或合并规则无效")?;
    let mapping = root
        .as_mapping()
        .context("请选择完整的 mihomo YAML 配置。")?;
    ensure!(
        mapping.keys().all(Value::is_string),
        "YAML 顶层字段必须是字符串。"
    );
    ensure!(
        mapping.contains_key("proxies") || mapping.contains_key("proxy-providers"),
        "配置缺少 proxies 或 proxy-providers。"
    );
    Ok(root)
}

#[derive(Clone, Debug)]
pub struct Summary {
    pub proxies: usize,
    pub groups: Vec<String>,
    pub rules: usize,
}
pub fn summary(yaml: &str) -> Result<Summary> {
    let root = parse(yaml)?;
    Ok(Summary {
        proxies: root["proxies"].as_sequence().map_or(0, Vec::len),
        groups: root["proxy-groups"]
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|g| g["name"].as_str().map(str::to_owned))
            .collect(),
        rules: root["rules"].as_sequence().map_or(0, Vec::len),
    })
}
fn set(map: &mut Mapping, key: &str, value: impl Into<Value>) {
    map.insert(key.into(), value.into());
}
/// An empty section such as `tun:` parses as null; treat it like a missing one.
fn section<'a>(map: &'a mut Mapping, key: &str) -> Result<&'a mut Mapping> {
    let value = map.entry(Value::from(key)).or_insert(Value::Null);
    if value.is_null() {
        *value = Value::Mapping(Mapping::new());
    }
    value
        .as_mapping_mut()
        .with_context(|| format!("{key} 必须是 YAML 对象。"))
}

pub fn runtime(yaml: &str, settings: &Settings, secret: &str) -> Result<String> {
    settings.validate()?;
    ensure!(!secret.is_empty(), "控制接口密钥不能为空。");
    let mut root = parse(yaml)?;
    let map = root.as_mapping_mut().unwrap();
    for name in [
        "external-ui",
        "external-ui-url",
        "external-ui-name",
        "external-controller-tls",
        "external-controller-unix",
        "external-controller-pipe",
        "external-controller-cors",
        "listeners",
        "tunnels",
        "tuic-server",
        "ss-config",
        "vmess-config",
        "tcptun-config",
        "udptun-config",
        "iptables",
        "authentication",
    ] {
        map.remove(name);
    }
    // Inbound servers use these suffixes (tuic-server, ss-config…); drop ones not listed above.
    map.retain(|key, _| {
        key.as_str()
            .is_none_or(|key| !key.ends_with("-server") && !key.ends_with("-config"))
    });
    set(map, "mixed-port", settings.proxy_port);
    for name in ["port", "socks-port", "redir-port", "tproxy-port"] {
        set(map, name, 0);
    }
    set(map, "allow-lan", false);
    set(map, "bind-address", "127.0.0.1");
    set(
        map,
        "external-controller",
        format!("127.0.0.1:{}", settings.controller_port),
    );
    set(map, "secret", secret);
    set(map, "mode", settings.mode.as_str());
    set(map, "log-level", "warning");
    set(map, "geodata-loader", "memconservative");
    // FlowBar's setting decides IPv6, whatever the profile says.
    set(map, "ipv6", settings.ipv6);
    let tun = section(map, "tun")?;
    set(tun, "enable", settings.tun);
    if settings.tun {
        set(tun, "device", TUN_DEVICE);
        set(tun, "auto-route", true);
        set(tun, "auto-detect-interface", true);
        tun.entry(Value::from("stack"))
            .or_insert(Value::from("mixed"));
        tun.entry(Value::from("dns-hijack"))
            .or_insert(serde_yaml_ng::to_value(["any:53", "tcp://any:53"])?);
        // Without IPv6 the TUN routes only IPv4. Strict routing makes IPv6 unreachable on
        // Linux and blocks it on Windows, so IPv6 traffic cannot bypass the proxy; macOS
        // has no strict routing, but its system DNS answers through the core.
        if !settings.ipv6 {
            set(tun, "strict-route", true);
        }
    }
    let dns = section(map, "dns")?;
    set(dns, "listen", format!("127.0.0.1:{}", settings.dns_port));
    set(dns, "ipv6", settings.ipv6);
    if settings.tun {
        set(dns, "enable", true);
        dns.entry(Value::from("nameserver"))
            .or_insert(serde_yaml_ng::to_value(["223.5.5.5", "1.1.1.1"])?);
    }
    Ok(serde_yaml_ng::to_string(&root)?)
}

#[cfg(any(target_os = "macos", test))]
pub fn dns_port(yaml: &str) -> Result<u16> {
    let root = parse(yaml)?;
    let listen = root["dns"]["listen"].as_str().context("DNS 监听地址无效")?;
    let port: u16 = listen.rsplit(':').next().unwrap_or_default().parse()?;
    ensure!(port >= 1024, "DNS 监听端口无效。");
    Ok(port)
}

pub fn seed_geo(source: &Path, destination: &Path) -> Result<()> {
    for name in GEO_NAMES {
        let origin = source.join(name);
        let target = destination.join(name);
        if origin.is_file() && !target.exists() {
            private_write(&target, &fs::read(origin)?)?;
        }
    }
    Ok(())
}
pub fn copy_providers(yaml: &str, source: &Path, destination: &Path) -> Result<()> {
    let root = parse(yaml)?;
    for kind in ["proxy-providers", "rule-providers"] {
        for (_, provider) in root[kind].as_mapping().into_iter().flatten() {
            let Some(path) = provider["path"].as_str() else {
                continue;
            };
            let relative = Path::new(path);
            ensure!(
                !relative.is_absolute()
                    && !path.starts_with('~')
                    && relative
                        .components()
                        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
                "provider 须使用配置目录内的相对路径：{path}"
            );
            let origin = source.join(relative);
            if !origin.exists() {
                if provider["type"].as_str() == Some("file") {
                    bail!("找不到 provider 文件：{path}");
                }
                continue;
            }
            ensure!(
                fs::symlink_metadata(&origin)?.is_file()
                    && origin.canonicalize()?.starts_with(source.canonicalize()?),
                "provider 文件须位于配置目录内，且不能是符号链接：{path}"
            );
            let target = destination.join(relative);
            private_dir(target.parent().unwrap())?;
            private_write(&target, &fs::read(&origin)?)?;
            // Keep the cache age so the core refreshes the provider on its real schedule.
            fs::File::options()
                .write(true)
                .open(&target)?
                .set_modified(fs::metadata(&origin)?.modified()?)?;
        }
    }
    Ok(())
}
pub fn import_profile(source: &Path, name: Option<&str>) -> Result<Settings> {
    // A bare relative file name has an empty parent, which breaks provider lookup.
    let source = &std::path::absolute(source).context("配置文件路径无效")?;
    let yaml = fs::read_to_string(source).context("无法读取 YAML 文件")?;
    summary(&yaml)?;
    private_dir(&home())?;
    let data = home().join("Data");
    private_dir(&data)?;
    let directory = source.parent().context("配置文件路径无效")?;
    seed_geo(directory, &data)?;
    if let Some(verge) = verge().filter(|verge| source.starts_with(verge)) {
        seed_geo(&verge, &data)?;
    }
    copy_providers(&yaml, directory, &data)?;
    let mut settings = Settings::load();
    // Importing a file again, for example after a subscription update, replaces its profile.
    let name = profile_name(
        &name
            .map(str::to_owned)
            .unwrap_or_else(|| source.file_stem().unwrap().to_string_lossy().into()),
    );
    private_dir(&profiles_dir())?;
    private_write(&profile_path(&name), yaml.as_bytes())?;
    settings.switch_profile(&name);
    // The new copy may have renamed its groups and nodes.
    settings.selections.clear();
    settings.mode = "rule".into();
    settings.save()?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    const FIXTURE: &str = "proxies:\n  - &node\n    name: 香港 / 01\n    password: '00123: on # keep'\n    server: example.com\n  - <<: *node\n    name: 东京\nproxy-groups:\n  - name: 代理\n    type: select\n    proxies: [香港 / 01, 东京]\nrules: [DOMAIN,example.com,代理]\ntun:\n  enable: true\n  stack: gvisor\n  route-exclude-address: [192.168.1.0/24]\ndns:\n  nameserver: [https://dns.alidns.com/dns-query]\n";
    #[test]
    fn preserves_merged_nodes_credentials_rules_and_dns() {
        let original = parse(FIXTURE).unwrap();
        let settings = Settings {
            tun: false,
            ..Settings::default()
        };
        let output = parse(&runtime(FIXTURE, &settings, "secret").unwrap()).unwrap();
        assert_eq!(output["proxies"], original["proxies"]);
        assert_eq!(output["rules"], original["rules"]);
        assert_eq!(output["proxy-groups"], original["proxy-groups"]);
        assert_eq!(output["dns"]["nameserver"], original["dns"]["nameserver"]);
        assert_eq!(output["tun"]["enable"].as_bool(), Some(false));
        assert_eq!(output["mixed-port"].as_u64(), Some(17890));
        assert_eq!(
            output["proxies"][1]["password"].as_str(),
            Some("00123: on # keep")
        );
    }
    #[test]
    fn tun_retains_exclusions_and_uses_selected_dns_port() {
        let output = runtime(FIXTURE, &Settings::default(), "secret").unwrap();
        let root = parse(&output).unwrap();
        assert_eq!(root["tun"]["stack"].as_str(), Some("gvisor"));
        assert_eq!(
            root["tun"]["route-exclude-address"][0].as_str(),
            Some("192.168.1.0/24")
        );
        assert_eq!(dns_port(&output).unwrap(), 15353);
    }
    #[test]
    fn ipv6_stays_off_unless_enabled() {
        let yaml = format!("ipv6: true\n{FIXTURE}  ipv6: true\n");
        let render = |settings: &Settings| parse(&runtime(&yaml, settings, "secret").unwrap());
        let off = render(&Settings::default()).unwrap();
        assert_eq!(off["ipv6"].as_bool(), Some(false));
        assert_eq!(off["dns"]["ipv6"].as_bool(), Some(false));
        assert_eq!(off["tun"]["strict-route"].as_bool(), Some(true));
        let on = render(&Settings {
            ipv6: true,
            ..Settings::default()
        })
        .unwrap();
        assert_eq!(on["ipv6"].as_bool(), Some(true));
        assert_eq!(on["dns"]["ipv6"].as_bool(), Some(true));
        assert!(on["tun"]["strict-route"].is_null());
        let proxy_only = render(&Settings {
            tun: false,
            ..Settings::default()
        })
        .unwrap();
        assert!(proxy_only["tun"]["strict-route"].is_null());
        // Settings saved before the option existed load with IPv6 off.
        let saved: Settings = serde_json::from_str(r#"{"tun":true}"#).unwrap();
        assert!(!saved.ipv6);
    }
    #[test]
    fn drops_extra_inbounds_and_accepts_empty_sections() {
        let yaml = "proxies: []\ntunnels: [tcp/udp,0.0.0.0:6553,1.1.1.1:53,DIRECT]\nss-config: ss://aes-128-gcm:pw@:8388\ntuic-server: {enable: true, listen: 0.0.0.0:443}\nfuture-server: {listen: 0.0.0.0:8443}\ntrojan-config: trojan://pw@:8444\ntun:\ndns:\n";
        let root = parse(&runtime(yaml, &Settings::default(), "secret").unwrap()).unwrap();
        for name in [
            "tunnels",
            "ss-config",
            "tuic-server",
            "future-server",
            "trojan-config",
        ] {
            assert!(root.get(name).is_none(), "{name} must not be kept");
        }
        assert_eq!(root["tun"]["enable"].as_bool(), Some(true));
        assert_eq!(root["dns"]["listen"].as_str(), Some("127.0.0.1:15353"));
    }
    #[test]
    fn prunes_only_old_ended_sessions() {
        let temporary = tempfile::tempdir().unwrap();
        let ids: Vec<_> = (0..8).map(|_| Uuid::new_v4().to_string()).collect();
        for (index, id) in ids.iter().enumerate() {
            let path = temporary.path().join(id);
            fs::create_dir(&path).unwrap();
            fs::write(path.join("config.yaml"), "secret: x").unwrap();
            let time = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(index as u64 * 60);
            let mut options = fs::File::options();
            options.read(true);
            // Windows opens directories only with backup semantics.
            #[cfg(windows)]
            std::os::windows::fs::OpenOptionsExt::custom_flags(&mut options, 0x0200_0000)
                .write(true);
            options.open(&path).unwrap().set_modified(time).unwrap();
        }
        fs::create_dir(temporary.path().join("not-a-session")).unwrap();
        prune_sessions(temporary.path(), |id| id == ids[0]);
        let exists = |id: &str| temporary.path().join(id).exists();
        let config = |id: &str| temporary.path().join(id).join("config.yaml").exists();
        assert!(exists(&ids[0]) && config(&ids[0]) && exists("not-a-session"));
        assert!(ids[1..4].iter().all(|id| !exists(id)));
        assert!(ids[4..].iter().all(|id| exists(id) && !config(id)));
    }
    #[test]
    fn rejects_duplicates_incomplete_yaml_and_bad_ports() {
        assert!(parse("proxies: []\nmode: rule\nmode: global").is_err());
        assert!(parse("- name: node").is_err());
        assert!(parse("rules: []").is_err());
        let settings = Settings {
            proxy_port: 19090,
            ..Settings::default()
        };
        assert!(runtime(FIXTURE, &settings, "secret").is_err());
    }
    #[test]
    fn settings_read_legacy_swift_camel_case() {
        let settings: Settings = serde_json::from_str(
            r#"{"profileName":"my-profile","autoConnect":true,"proxyPort":17891}"#,
        )
        .unwrap();
        assert_eq!(settings.profile_name, "my-profile");
        assert!(settings.auto_connect);
        assert_eq!(settings.proxy_port, 17891);
        assert!(settings.tun);
    }
    #[test]
    fn profiles_keep_their_own_node_choices() {
        let mut settings = Settings {
            profile_name: "工作".into(),
            ..Settings::default()
        };
        settings.selections.insert("代理".into(), "香港".into());
        settings.switch_profile("家里");
        assert_eq!(settings.profile_name, "家里");
        assert!(settings.selections.is_empty());
        settings.selections.insert("代理".into(), "东京".into());
        settings.switch_profile("工作");
        assert_eq!(settings.selections["代理"], "香港");
        assert_eq!(settings.profile_selections["家里"]["代理"], "东京");
        assert!(!settings.profile_selections.contains_key("工作"));
        // Settings saved before profiles existed keep their node choices.
        let saved: Settings =
            serde_json::from_str(r#"{"profileName":"x","selections":{"a":"b"}}"#).unwrap();
        assert!(saved.profile_selections.is_empty() && saved.selections["a"] == "b");
    }
    #[test]
    fn lists_profiles_with_portable_names() {
        let temporary = tempfile::tempdir().unwrap();
        for name in ["b.yaml", "a.yaml", "notes.txt", ".yaml", ".flowbar-x"] {
            fs::write(temporary.path().join(name), "").unwrap();
        }
        fs::create_dir(temporary.path().join("folder.yaml")).unwrap();
        assert_eq!(list_profiles(temporary.path()), ["a", "b"]);
        assert!(list_profiles(&temporary.path().join("missing")).is_empty());
        assert_eq!(profile_name(" a/b:c\\d "), "a_b_c_d");
        assert_eq!(profile_name("  "), "配置");
        assert_eq!(profile_name("香港 · 01"), "香港 · 01");
    }
    #[test]
    fn imports_provider_and_rejects_traversal_and_symlinks() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let target = temporary.path().join("target");
        private_dir(&source.join("rules")).unwrap();
        private_dir(&target).unwrap();
        fs::write(source.join("rules/test.yaml"), "payload: [example.com]").unwrap();
        let age = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(86400);
        fs::File::options()
            .write(true)
            .open(source.join("rules/test.yaml"))
            .unwrap()
            .set_modified(age)
            .unwrap();
        let yaml =
            "proxies: []\nrule-providers:\n  local:\n    type: file\n    path: ./rules/test.yaml\n";
        copy_providers(yaml, &source, &target).unwrap();
        let copied = fs::metadata(target.join("rules/test.yaml")).unwrap();
        assert!(copied.is_file() && copied.modified().unwrap() == age);
        assert!(copy_providers(&yaml.replace("./rules/", "../"), &source, &target).is_err());
        // Creating a symlink on Windows needs developer mode or administrator rights.
        #[cfg(unix)]
        std::os::unix::fs::symlink(source.join("rules/test.yaml"), source.join("link.yaml"))
            .unwrap();
        assert!(
            copy_providers(
                &yaml.replace("./rules/test.yaml", "link.yaml"),
                &source,
                &target
            )
            .is_err()
        );
    }
}
