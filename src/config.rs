use crate::subscription::Subscription;
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
/// macOS names TUN devices `utun<N>` and mihomo replaces any other name with the next free
/// one, so FlowBar leaves the choice to the core there.
#[cfg(not(target_os = "macos"))]
const TUN_DEVICE: &str = "FlowBar";
/// Proxy types the bundled mihomo cannot load: rill's XPro, which FlowBar 0.5 to 0.8 ran.
const UNSUPPORTED_PROXY_TYPES: &[&str] = &["xpro"];
/// Policies mihomo defines itself, which groups and rules may name.
const BUILTIN_POLICIES: &[&str] = &[
    "DIRECT",
    "REJECT",
    "REJECT-DROP",
    "COMPATIBLE",
    "PASS",
    "PASS-RULE",
    "GLOBAL",
];
/// The log level when the profile sets none, mihomo's own default.
const DEFAULT_LOG_LEVEL: &str = "info";

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
    /// The links of profiles imported from a subscription, by profile name.
    pub subscriptions: BTreeMap<String, Subscription>,
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
            subscriptions: BTreeMap::new(),
        }
    }
}
impl Settings {
    pub fn load() -> Self {
        let path = home().join("settings.json");
        let mut settings = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                // The next save would replace a damaged file, and with it every subscription
                // link and node choice; keep it for the user to recover them.
                let _ = fs::rename(&path, home().join("settings.damaged.json"));
                Settings::default()
            }),
            Err(_) => Settings::default(),
        };
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
        self.subscriptions.remove(&self.profile_name);
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
/// A profile name that is also a file name: no path separators or characters Windows
/// forbids, on Windows no device name such as `CON` or `nul.backup`, and short enough for
/// the file name limit once `.yaml` is added.
pub fn profile_name(name: &str) -> String {
    /// Bytes, below the 255 that file systems allow for one name.
    const MAX_BYTES: usize = 200;
    let mut name: String = name
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
    if name.len() > MAX_BYTES {
        let mut end = MAX_BYTES;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
        name.truncate(name.trim_end().len());
    }
    // Windows reads the part before the first dot as a device name, ignoring trailing spaces.
    let device = name.split('.').next().unwrap_or_default().trim_end();
    let reserved = cfg!(windows) && device_name(device);
    if reserved {
        name.insert(device.len(), '_');
    }
    if name.is_empty() {
        "配置".into()
    } else {
        name
    }
}
/// Names Windows reserves for devices, in any case.
fn device_name(name: &str) -> bool {
    ["CON", "PRN", "AUX", "NUL"]
        .iter()
        .any(|d| name.eq_ignore_ascii_case(d))
        || name.len() == 4
            && name.as_bytes()[3].is_ascii_digit()
            && name.get(..3).is_some_and(|prefix| {
                ["COM", "LPT"]
                    .iter()
                    .any(|d| prefix.eq_ignore_ascii_case(d))
            })
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

#[cfg(test)]
thread_local! {
    /// A test's private data directory, so tests need not change the process environment.
    pub static TEST_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}
pub fn home() -> PathBuf {
    #[cfg(test)]
    if let Some(home) = TEST_HOME.with(|home| home.borrow().clone()) {
        return home;
    }
    user_home()
}
#[cfg(unix)]
fn user_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is required")).join(DATA_DIR)
}
#[cfg(windows)]
fn user_home() -> PathBuf {
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
        // launchd starts the privileged helper with umask 077. Status files still
        // need the explicitly requested 0644 so the unprivileged UI can read them.
        // Set permissions on the open descriptor, before publishing the file.
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(mode))?;
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
    Ok(render(yaml, settings, secret)?.yaml)
}
/// A runtime config and what FlowBar changed in the profile to make the core load it.
pub struct Runtime {
    pub yaml: String,
    /// One line per kind of repair, for the session log and the connection progress.
    pub notes: Vec<String>,
}
pub fn render(yaml: &str, settings: &Settings, secret: &str) -> Result<Runtime> {
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
    let notes = repair(map);
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
    // The profile's own log level; a missing or invalid one gets mihomo's default.
    let level = map
        .get("log-level")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .filter(|level| ["silent", "error", "warning", "info", "debug"].contains(&level.as_str()))
        .unwrap_or_else(|| DEFAULT_LOG_LEVEL.into());
    set(map, "log-level", level);
    set(map, "geodata-loader", "memconservative");
    // FlowBar's setting decides IPv6, whatever the profile says.
    set(map, "ipv6", settings.ipv6);
    if settings.tun {
        // A pinned interface overrides the TUN's detection of the live default route; once
        // the network changes, every dial, the proxy servers' included, would leave through
        // the wrong interface or the TUN itself.
        map.remove("interface-name");
        bypass_proxy_servers(map);
    }
    let tun = section(map, "tun")?;
    set(tun, "enable", settings.tun);
    if settings.tun {
        #[cfg(not(target_os = "macos"))]
        set(tun, "device", TUN_DEVICE);
        #[cfg(target_os = "macos")]
        if tun
            .get("device")
            .and_then(Value::as_str)
            .is_none_or(|device| !valid_utun(device))
        {
            tun.remove("device");
        }
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
    Ok(Runtime {
        yaml: serde_yaml_ng::to_string(&root)?,
        notes,
    })
}
/// The names mihomo accepts as a TUN device on macOS: `utun` and a number.
#[cfg(any(target_os = "macos", test))]
fn valid_utun(device: &str) -> bool {
    device
        .strip_prefix("utun")
        .is_some_and(|index| !index.is_empty() && index.parse::<i16>().is_ok())
}

fn name_of(value: &Value) -> Option<String> {
    match value {
        Value::String(name) => Some(name.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}
/// A few names, for a note that should stay one readable line.
fn listed(names: &[String]) -> String {
    const SHOWN: usize = 5;
    let mut text = names
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join("、");
    if names.len() > SHOWN {
        text.push_str(&format!(" 等 {} 个", names.len()));
    }
    text
}
/// The policy a rule sends its traffic to, found the way mihomo splits a rule.
fn rule_target(rule: &str) -> Option<(String, String)> {
    let items: Vec<&str> = rule.split(',').map(str::trim).collect();
    let kind = items[0].to_ascii_uppercase();
    let target = match kind.as_str() {
        "MATCH" => items.get(1)?,
        "NOT" | "OR" | "AND" | "SUB-RULE" | "DOMAIN-REGEX" | "PROCESS-NAME-REGEX"
        | "PROCESS-PATH-REGEX" => items.last().filter(|_| items.len() > 1)?,
        _ => items.get(2)?,
    };
    Some((kind, (*target).to_owned()))
}

/// Retain an exact destination IP rule when its dedicated policy disappeared.
/// Do not redirect source-IP rules or whole subnets to a different policy.
fn ip_rule_fallback(rule: &str) -> Option<String> {
    let mut fields = rule.splitn(4, ',').map(str::trim);
    let kind = fields.next()?;
    let payload = fields.next()?;
    let _target = fields.next()?;
    let options = fields.next();
    if options.is_some_and(|options| options.split(',').any(|s| s.trim() == "src")) {
        return None;
    }
    let (address, prefix) = payload.split_once('/')?;
    let address = address.parse::<std::net::IpAddr>().ok()?;
    let host = matches!(
        (kind.to_ascii_uppercase().as_str(), address, prefix),
        ("IP-CIDR", std::net::IpAddr::V4(_), "32")
            | ("IP-CIDR" | "IP-CIDR6", std::net::IpAddr::V6(_), "128")
    );
    host.then(|| match options {
        Some(options) => format!("{kind},{payload},proxy,{options}"),
        None => format!("{kind},{payload},proxy"),
    })
}

/// Makes a profile whose nodes were edited or deleted load again, without touching the
/// imported copy. mihomo refuses a whole config over one unknown proxy type, one group member
/// or rule target that no longer exists, or one group left without members. FlowBar drops
/// what it cannot run and reports each kind of change.
fn repair(map: &mut Mapping) -> Vec<String> {
    let mut notes = vec![];
    let mut proxies = vec![];
    if let Some(list) = map.get_mut("proxies").and_then(Value::as_sequence_mut) {
        let mut skipped = vec![];
        list.retain(|proxy| {
            let unsupported = proxy["type"].as_str().is_some_and(|kind| {
                UNSUPPORTED_PROXY_TYPES.contains(&kind.to_ascii_lowercase().as_str())
            });
            if unsupported {
                skipped.push(name_of(&proxy["name"]).unwrap_or_else(|| "未命名".into()));
            }
            !unsupported
        });
        if !skipped.is_empty() {
            notes.push(format!(
                "已跳过 {} 个 mihomo 不支持的 XPro 节点：{}",
                skipped.len(),
                listed(&skipped)
            ));
        }
        proxies.extend(list.iter().filter_map(|proxy| name_of(&proxy["name"])));
        // rill accepted Xray's Mux.Cool here, but mihomo's smux is sing-mux and
        // rejects it while loading the node. VLESS/VMess also work without this
        // optional layer. Do not substitute smux/h2mux: the server speaks a
        // different wire protocol. Leave supported multiplexers untouched.
        let mut without_mux = vec![];
        for proxy in list.iter_mut() {
            let xray = matches!(proxy["type"].as_str(), Some("vless" | "vmess"));
            let muxcool = proxy["smux"]["protocol"].as_str() == Some("muxcool");
            if xray && muxcool {
                without_mux.push(name_of(&proxy["name"]).unwrap_or_else(|| "未命名".into()));
                proxy.as_mapping_mut().unwrap().remove("smux");
            }
        }
        if !without_mux.is_empty() {
            notes.push(format!(
                "已关闭 {} 个节点上 mihomo 不支持的 Mux.Cool 多路复用，保留普通 VLESS/VMess 连接：{}",
                without_mux.len(),
                listed(&without_mux)
            ));
        }
    }
    let mut dangling = 0;
    let mut touched = std::collections::BTreeSet::new();
    let mut removed = vec![];
    if let Some(groups) = map.get_mut("proxy-groups").and_then(Value::as_sequence_mut) {
        // Removing an emptied group can empty the groups that only listed it.
        loop {
            let known: std::collections::HashSet<String> = proxies
                .iter()
                .cloned()
                .chain(groups.iter().filter_map(|group| name_of(&group["name"])))
                .chain(BUILTIN_POLICIES.iter().map(|name| (*name).to_owned()))
                .collect();
            for group in groups.iter_mut() {
                let name = name_of(&group["name"]).unwrap_or_default();
                let Some(members) = group.get_mut("proxies").and_then(Value::as_sequence_mut)
                else {
                    continue;
                };
                let before = members.len();
                members.retain(|member| name_of(member).is_none_or(|m| known.contains(&m)));
                if members.len() != before {
                    dangling += before - members.len();
                    touched.insert(name);
                }
            }
            let before = groups.len();
            groups.retain(|group| {
                let members = group["proxies"]
                    .as_sequence()
                    .is_some_and(|m| !m.is_empty());
                let providers = group["use"].as_sequence().is_some_and(|u| !u.is_empty());
                let includes = [
                    "include-all",
                    "include-all-proxies",
                    "include-all-providers",
                ]
                .iter()
                .any(|key| group[*key].as_bool() == Some(true));
                let empty = !members && !providers && !includes;
                if empty {
                    removed.push(name_of(&group["name"]).unwrap_or_default());
                }
                !empty
            });
            if groups.len() == before {
                break;
            }
        }
    }
    if dangling > 0 {
        notes.push(format!(
            "已移除 {dangling} 处指向不存在节点或策略组的引用，涉及 {} 个策略组",
            touched.len()
        ));
    }
    if !removed.is_empty() {
        notes.push(format!("已停用没有可用节点的策略组：{}", listed(&removed)));
    }
    let known: std::collections::HashSet<String> = proxies
        .into_iter()
        .chain(
            map.get("proxy-groups")
                .and_then(Value::as_sequence)
                .into_iter()
                .flatten()
                .filter_map(|group| name_of(&group["name"])),
        )
        .chain(BUILTIN_POLICIES.iter().map(|name| (*name).to_owned()))
        .collect();
    let sub_rules: std::collections::HashSet<String> = map
        .get("sub-rules")
        .and_then(Value::as_mapping)
        .into_iter()
        .flatten()
        .filter_map(|(name, _)| name_of(name))
        .collect();
    let mut dropped = 0;
    let mut fallback = 0;
    let mut keep = |rule: &mut Value| {
        let Some((kind, target)) = rule.as_str().and_then(rule_target) else {
            return true;
        };
        let valid = if kind == "SUB-RULE" {
            sub_rules.contains(&target) || known.contains(&target)
        } else {
            known.contains(&target)
        };
        if !valid {
            if known.contains("proxy") {
                if let Some(replacement) = rule.as_str().and_then(ip_rule_fallback) {
                    *rule = Value::String(replacement);
                    fallback += 1;
                    return true;
                }
            }
            dropped += 1;
        }
        valid
    };
    if let Some(rules) = map.get_mut("rules").and_then(Value::as_sequence_mut) {
        rules.retain_mut(&mut keep);
    }
    if let Some(subs) = map.get_mut("sub-rules").and_then(Value::as_mapping_mut) {
        for (_, rules) in subs.iter_mut() {
            if let Some(rules) = rules.as_sequence_mut() {
                rules.retain_mut(&mut keep);
            }
        }
    }
    if dropped > 0 {
        notes.push(format!("已跳过 {dropped} 条指向不存在节点或策略组的规则"));
    }
    if fallback > 0 {
        notes.push(format!(
            "已将 {fallback} 条对应节点或策略组不存在的服务器 IP 规则改为使用 proxy"
        ));
    }
    notes
}

/// Connections to a proxy endpoint go out directly if a dial misses the physical
/// interface. Match its port as well: bypassing the whole IP would also force unrelated
/// services (such as SSH on the same machine) to go direct instead of following the profile.
fn bypass_proxy_servers(map: &mut Mapping) {
    let mut seen = std::collections::HashSet::new();
    let bypass: Vec<Value> = map
        .get("proxies")
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
        // A server behind dialer-proxy is reached through another node, never directly.
        .filter(|proxy| proxy.get("dialer-proxy").is_none())
        .filter_map(|proxy| {
            let server = proxy["server"]
                .as_str()?
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']');
            if server.is_empty() || server.contains([',', '(', ')']) {
                return None;
            }
            // Hysteria port hopping overrides `port`. DST-PORT uses `/` between
            // ranges, whereas the outbound's `ports` field uses commas.
            let ports = name_of(proxy.get("ports").unwrap_or(&proxy["port"]))?;
            // Profiles often write `ports: 443, 8443-8450`.
            let ports: String = ports
                .replace(',', "/")
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            let valid = ports.split('/').all(|range| {
                let mut bounds = range.split('-');
                let port = |s: &str| s.parse::<u16>().ok().filter(|p| *p > 0);
                let Some(start) = bounds.next().and_then(port) else {
                    return false;
                };
                let end = bounds.next().map(port).unwrap_or(Some(start));
                end.is_some_and(|end| end >= start) && bounds.next().is_none()
            });
            if !valid {
                return None;
            }
            let host = match server.parse::<std::net::IpAddr>() {
                Ok(std::net::IpAddr::V4(ip)) => format!("IP-CIDR,{ip}/32,no-resolve"),
                Ok(std::net::IpAddr::V6(ip)) => format!("IP-CIDR6,{ip}/128,no-resolve"),
                Err(_) => format!("DOMAIN,{server}"),
            };
            Some(format!("AND,(({host}),(DST-PORT,{ports})),DIRECT"))
        })
        .filter(|rule| seen.insert(rule.to_ascii_lowercase()))
        .map(Value::from)
        .collect();
    if bypass.is_empty() {
        return;
    }
    match map.get_mut("rules").and_then(Value::as_sequence_mut) {
        Some(rules) => {
            rules.splice(0..0, bypass);
        }
        None => {
            map.insert("rules".into(), Value::Sequence(bypass));
        }
    }
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
/// Saves a profile's YAML, which the caller has checked, as the named profile's copy.
pub fn store_profile(name: &str, yaml: &str) -> Result<()> {
    private_dir(&home())?;
    private_dir(&profiles_dir())?;
    private_write(&profile_path(name), yaml.as_bytes())
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
    store_profile(&name, &yaml)?;
    // A local file replaces the profile's content, so its subscription would overwrite it.
    settings.subscriptions.remove(&name);
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
    fn repairs_deleted_and_xpro_nodes_without_touching_the_rest() {
        let yaml = "proxies:\n  - {name: 东京, type: ss, server: jp.example.com, port: 443}\n  - {name: 旧, type: XPro, server: 1.2.3.4, port: 443}\nproxy-groups:\n  - {name: 代理, type: select, proxies: [香港, 东京, 旧, 专线, DIRECT]}\n  - {name: 专线, type: select, proxies: [旧, 已删除]}\n  - {name: 只有专线, type: url-test, proxies: [专线]}\n  - {name: 订阅, type: select, use: [sub]}\n  - {name: 全部, type: select, include-all: true, proxies: [已删除]}\nproxy-providers:\n  sub: {type: http, url: https://example.com/sub, path: ./sub.yaml}\nsub-rules:\n  内部: ['DOMAIN,a.com,专线', 'MATCH,代理']\nrules:\n  - DOMAIN,b.com,专线\n  - AND,((DOMAIN,c.com),(NETWORK,UDP)),只有专线\n  - DOMAIN-REGEX,^(d|e)\\.com$,代理\n  - SUB-RULE,(DOMAIN,f.com),内部\n  - GEOIP,CN,DIRECT,no-resolve\n  - MATCH,代理\n";
        let output = render(
            yaml,
            &Settings {
                tun: false,
                ..Settings::default()
            },
            "secret",
        )
        .unwrap();
        let root = parse(&output.yaml).unwrap();
        let names = |value: &Value| -> Vec<String> {
            value
                .as_sequence()
                .unwrap()
                .iter()
                .filter_map(name_of)
                .collect()
        };
        assert_eq!(root["proxies"].as_sequence().unwrap().len(), 1);
        let groups: Vec<_> = root["proxy-groups"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|g| g["name"].as_str().unwrap().to_owned())
            .collect();
        // 专线 lost all its nodes, and 只有专线 listed nothing else.
        assert_eq!(groups, ["代理", "订阅", "全部"]);
        assert_eq!(
            names(&root["proxy-groups"][0]["proxies"]),
            ["东京", "DIRECT"]
        );
        assert!(names(&root["proxy-groups"][2]["proxies"]).is_empty());
        assert_eq!(
            names(&root["rules"]),
            [
                "DOMAIN-REGEX,^(d|e)\\.com$,代理",
                "SUB-RULE,(DOMAIN,f.com),内部",
                "GEOIP,CN,DIRECT,no-resolve",
                "MATCH,代理"
            ]
        );
        assert_eq!(names(&root["sub-rules"]["内部"]), ["MATCH,代理"]);
        assert_eq!(output.notes.len(), 4, "{:?}", output.notes);
        assert!(output.notes[0].contains("XPro") && output.notes[0].contains("旧"));
        // An intact profile is left as it is.
        let intact = render(
            FIXTURE,
            &Settings {
                tun: false,
                ..Settings::default()
            },
            "secret",
        )
        .unwrap();
        assert!(intact.notes.is_empty());
    }
    #[test]
    fn missing_server_policy_falls_back_to_proxy_without_changing_other_rules() {
        let yaml = "proxies:\n  - {name: node, type: ss, server: 203.0.113.9, port: 443}\n  - {name: old, type: xpro, server: 203.0.113.10, port: 443}\nproxy-groups:\n  - {name: proxy, type: select, proxies: [node]}\n  - {name: dedicated, type: select, proxies: [old]}\nsub-rules:\n  internal: ['IP-CIDR,192.0.2.2/32,deleted,no-resolve']\nrules:\n  - IP-CIDR,203.0.113.9/32,node,no-resolve\n  - IP-CIDR,203.0.113.10/32,old,no-resolve\n  - IP-CIDR,203.0.113.11/32,dedicated\n  - IP-CIDR6,2001:db8::1/128,deleted,no-resolve\n  - IP-CIDR,192.0.2.1/32,DIRECT,no-resolve\n  - IP-CIDR,198.51.100.0/24,deleted,no-resolve\n  - IP-CIDR,198.51.100.1/32,deleted,src,no-resolve\n  - DOMAIN,example.com,deleted\n  - MATCH,proxy\n";
        for tun in [false, true] {
            let output = render(
                yaml,
                &Settings {
                    tun,
                    ..Settings::default()
                },
                "secret",
            )
            .unwrap();
            let root = parse(&output.yaml).unwrap();
            let rules: Vec<_> = root["rules"]
                .as_sequence()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .filter(|rule| !rule.starts_with("AND,"))
                .collect();
            assert_eq!(
                rules,
                [
                    "IP-CIDR,203.0.113.9/32,node,no-resolve",
                    "IP-CIDR,203.0.113.10/32,proxy,no-resolve",
                    "IP-CIDR,203.0.113.11/32,proxy",
                    "IP-CIDR6,2001:db8::1/128,proxy,no-resolve",
                    "IP-CIDR,192.0.2.1/32,DIRECT,no-resolve",
                    "MATCH,proxy",
                ]
            );
            assert_eq!(
                root["sub-rules"]["internal"][0].as_str(),
                Some("IP-CIDR,192.0.2.2/32,proxy,no-resolve")
            );
            assert!(
                output
                    .notes
                    .iter()
                    .any(|note| note.contains("4 条") && note.contains("proxy"))
            );
        }
        // Without a surviving proxy policy, do not invent a dangling fallback.
        let without_proxy = yaml
            .replace("name: proxy,", "name: other,")
            .replace("MATCH,proxy", "MATCH,other");
        let root =
            parse(&runtime(&without_proxy, &Settings::default(), "secret").unwrap()).unwrap();
        assert!(
            root["rules"]
                .as_sequence()
                .unwrap()
                .iter()
                .all(|rule| !rule.as_str().unwrap().contains(",proxy"))
        );
        assert!(
            root["sub-rules"]["internal"]
                .as_sequence()
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn muxcool_keeps_the_node_and_supported_multiplexers_stay_intact() {
        let yaml = "proxies:\n  - {name: cf, type: vless, server: cf.example.com, uuid: keep, network: ws, ws-opts: {path: /keep}, smux: {enabled: true, protocol: muxcool}}\n  - {name: vm, type: vmess, smux: {enabled: true, protocol: muxcool}}\n  - {name: off, type: vless, smux: {enabled: false, protocol: muxcool}}\n  - {name: sing, type: vless, smux: {enabled: true, protocol: smux, padding: true}}\n  - {name: future, type: vless, smux: {enabled: true, protocol: unknown}}\nproxy-groups:\n  - {name: proxy, type: select, proxies: [cf, vm, off, sing, future]}\nrules: ['MATCH,proxy']\n";
        let original = parse(yaml).unwrap();
        for tun in [false, true] {
            let result = render(
                yaml,
                &Settings {
                    tun,
                    ..Settings::default()
                },
                "secret",
            )
            .unwrap();
            let root = parse(&result.yaml).unwrap();
            assert_eq!(root["proxies"].as_sequence().unwrap().len(), 5);
            for index in 0..3 {
                let mut expected = original["proxies"][index].clone();
                expected.as_mapping_mut().unwrap().remove("smux");
                assert_eq!(root["proxies"][index], expected);
            }
            for index in 3..5 {
                assert_eq!(root["proxies"][index], original["proxies"][index]);
            }
            assert_eq!(root["proxy-groups"], original["proxy-groups"]);
            assert_eq!(result.notes.len(), 1);
            assert!(result.notes[0].contains("Mux.Cool"));
        }
    }
    #[test]
    fn tun_sends_proxy_servers_direct_and_follows_the_live_interface() {
        let yaml = "interface-name: en0\nlog-level: warning\nproxies:\n  - {name: a, type: ss, server: a.example.com, port: 443}\n  - {name: b, type: ss, server: 203.0.113.7, port: 443}\n  - {name: c, type: ss, server: '2001:db8::1', port: 443}\n  - {name: d, type: ss, server: A.example.com, port: 443}\n  - {name: e, type: ss, server: inner.example.com, port: 443, dialer-proxy: a}\n  - {name: hop, type: hysteria2, server: 203.0.113.7, port: 443, ports: '8443, 9000-9010'}\n  - {name: bad, type: ss, server: invalid.example.com, port: 0}\nrules: ['MATCH,a']\n";
        let root = parse(&runtime(yaml, &Settings::default(), "secret").unwrap()).unwrap();
        let rules: Vec<_> = root["rules"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap())
            .collect();
        assert_eq!(
            rules,
            [
                "AND,((DOMAIN,a.example.com),(DST-PORT,443)),DIRECT",
                "AND,((IP-CIDR,203.0.113.7/32,no-resolve),(DST-PORT,443)),DIRECT",
                "AND,((IP-CIDR6,2001:db8::1/128,no-resolve),(DST-PORT,443)),DIRECT",
                "AND,((IP-CIDR,203.0.113.7/32,no-resolve),(DST-PORT,8443/9000-9010)),DIRECT",
                "MATCH,a"
            ]
        );
        assert!(root.get("interface-name").is_none());
        assert_eq!(root["log-level"].as_str(), Some("warning"));
        let proxy = parse(
            &runtime(
                yaml,
                &Settings {
                    tun: false,
                    ..Settings::default()
                },
                "secret",
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(proxy["rules"].as_sequence().unwrap().len(), 1);
        assert_eq!(proxy["interface-name"].as_str(), Some("en0"));
        let unset = parse(&runtime(FIXTURE, &Settings::default(), "secret").unwrap()).unwrap();
        assert_eq!(unset["log-level"].as_str(), Some("info"));
    }
    #[test]
    fn macos_tun_names_follow_utun_numbering() {
        assert!(valid_utun("utun4") && valid_utun("utun1024"));
        assert!(!valid_utun("utun") && !valid_utun("FlowBar") && !valid_utun("utunx"));
        let root = parse(&runtime(FIXTURE, &Settings::default(), "secret").unwrap()).unwrap();
        #[cfg(target_os = "macos")]
        assert!(root["tun"].get("device").is_none());
        #[cfg(not(target_os = "macos"))]
        assert_eq!(root["tun"]["device"].as_str(), Some(TUN_DEVICE));
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
    fn damaged_settings_are_kept_aside_instead_of_overwritten() {
        let temporary = tempfile::tempdir().unwrap();
        TEST_HOME.with(|home| *home.borrow_mut() = Some(temporary.path().into()));
        let damaged = r#"{"profileName":"x","subscriptions":{"x":{"url":"https://a"#;
        fs::write(temporary.path().join("settings.json"), damaged).unwrap();
        let settings = Settings::load();
        assert_eq!(settings.profile_name, Settings::default().profile_name);
        settings.save().unwrap();
        assert_eq!(
            fs::read_to_string(temporary.path().join("settings.damaged.json")).unwrap(),
            damaged
        );
        // A readable file loads as before.
        assert_eq!(Settings::load().profile_name, settings.profile_name);
        assert!(temporary.path().join("settings.damaged.json").exists());
        TEST_HOME.with(|home| *home.borrow_mut() = None);
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
        // Windows device names cannot be file names, with or without an extension.
        assert!(
            ["CON", "nul", "com1", "LPT9", "Aux"]
                .iter()
                .all(|n| device_name(n))
        );
        assert!(
            !["console", "com", "com10", "aé1", ""]
                .iter()
                .any(|n| device_name(n))
        );
        if cfg!(windows) {
            assert_eq!(profile_name("CON"), "CON_");
            assert_eq!(profile_name("nul.backup"), "nul_.backup");
            assert_eq!(profile_name("Lpt9 .x"), "Lpt9_ .x");
        } else {
            assert_eq!(profile_name("CON"), "CON");
        }
        assert_eq!(profile_name("console.x"), "console.x");
        // Long names are cut at a character boundary below the file name limit.
        let long = profile_name(&"节点".repeat(100));
        assert!(long.len() <= 200 && long.len() > 190);
        assert!(long.chars().all(|c| c == '节' || c == '点'));
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
