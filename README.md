# FlowBar

用 Rust 编写的 macOS 原生菜单栏代理工具，使用独立的 [mihomo](https://github.com/MetaCubeX/mihomo) 内核按 YAML 规则分流。通过 objc2 调用 AppKit 和 ServiceManagement，无 WebView。

支持 macOS 13 及以上，已在 Apple Silicon 上构建和验证。

## 功能

- 导入完整的 mihomo YAML，保留节点、策略组、分流规则和 DNS 上游。
- TUN 接管应用流量；也可关闭 TUN，仅使用本地 HTTP/SOCKS 代理。
- 菜单栏切换规则、全局、直连模式，以及策略组和节点。
- 原生登录自启，并可独立设置打开应用后自动连接。
- 从 GitHub Releases 检查更新，在应用内下载、校验并替换后自动重启。
- 退出应用或主进程意外退出时，监督进程停止内核。
- 本地日志和配置检查，控制接口仅监听回环地址并使用随机密钥。

界面冷启动空闲时，Apple Silicon 本机测得约 11 MB 内存 footprint，**不含 mihomo 内核**。总占用取决于配置、规则、流量和 TUN 协议栈。应用没有网页面板、实时流量图或日志订阅；仅在需要时查询策略组。

## 构建与安装

需要 Rust 1.85+、Apple Command Line Tools、Python 3，以及适合本机架构的 mihomo 可执行文件。

```sh
git clone git@github.com:best-shuke/FlowBar.git
cd FlowBar
./scripts/build.sh /absolute/path/to/mihomo
mkdir -p ~/Applications
cp -R dist/FlowBar.app ~/Applications/
open ~/Applications/FlowBar.app
```

也可直接执行 `./scripts/build.sh`，脚本依次查找已安装的 FlowBar 内核、Clash Verge 内核或 PATH 中的 mihomo。内核会复制到应用包中，运行时不依赖 Clash Verge。

构建脚本生成并验证本地 ad hoc 签名。仓库只保存源码、图标和许可；不包含 mihomo 二进制、个人配置、订阅、节点凭据或 Geo 数据。

## 使用

1. 打开 FlowBar，从菜单或设置导入自己的 YAML，例如 `zgo-or.yaml`。
2. 使用 TUN 前，退出其他正在接管网络的代理客户端，避免路由冲突。
3. 点击「启动连接」，按系统提示授权管理员权限。
4. 在菜单栏选择分流模式、策略组和节点；关闭设置窗口后仍在菜单栏运行。

「登录时打开 FlowBar」使用 macOS 原生登录项；系统要求批准时会打开登录项设置。「打开 FlowBar 后自动连接」是独立开关。TUN 当前没有常驻 root 服务，登录后自动连接仍可能显示管理员授权窗口。

默认开启 TUN、关闭自动连接。关闭 TUN 时，本版不自动修改系统代理，需要在客户端中指定本地代理端口。

| 用途 | 默认地址 |
| --- | --- |
| HTTP / SOCKS | `127.0.0.1:17890` |
| 控制接口 | `127.0.0.1:19090` |
| DNS | `127.0.0.1:15353` |

端口可以在设置中修改。

## 检查更新

菜单或设置窗口中的「检查更新…」查询 GitHub 上最新的正式 Release。「自动检查更新」默认开启，应用打开约 10 秒后检查一次，之后每天检查一次；自动检查失败时不弹窗，下个小时重试。

发现新版本后，点击「安装并重启」会把对应架构的 `FlowBar-<版本>-macos-arm64.zip` 下载到应用所在目录，然后依次校验：大小、GitHub 提供的 SHA-256、代码签名、Bundle ID 和版本号，并确认新程序能在本机运行。校验通过后原子替换当前应用，断开连接并重新打开。本地 HTTP 代理运行时，更新请求走该代理；TUN 运行时由 TUN 接管。

应用需放在可写目录中（例如 `~/Applications`）。开发时直接运行的 `target/release/flowbar` 不能自我更新。命令行可用 `FlowBar.app/Contents/MacOS/FlowBar --check-update` 检查版本。

签名仍是 ad hoc，安装包的可信度来自 GitHub 的 HTTPS 和 Release 本身，没有开发者身份校验。应用内下载不会带隔离标记，所以更新后不会被 Gatekeeper 拦截。但第一次用浏览器下载 Release 安装时会被拦截，可在「系统设置 → 隐私与安全性」中允许，或改用终端下载：

```sh
curl -fLO https://github.com/best-shuke/FlowBar/releases/latest/download/FlowBar-<版本>-macos-arm64.zip
```

### 发布新版本

仓库须为公开，应用才能匿名读取 Release。发布步骤：

1. 同时修改 `Cargo.toml` 的 `version`、`Resources/Info.plist` 的 `CFBundleShortVersionString`（并递增 `CFBundleVersion`），执行 `cargo build` 更新 `Cargo.lock`；`cargo test` 会检查前两者一致。
2. 提交后打标签并推送：`git tag v0.3.0 && git push origin main v0.3.0`。
3. `.github/workflows/release.yml` 在 macOS runner 上核对标签与版本、下载并校验固定版本的 mihomo（`MIHOMO_VERSION`）、运行测试、构建并发布 Release。

升级内置 mihomo 时，同时修改工作流中的 `MIHOMO_VERSION` 和 `MIHOMO_SHA256`。

## 配置与数据

用户数据位于 `~/Library/Application Support/FlowBar/`：

| 路径 | 内容 |
| --- | --- |
| `profile.yaml` | 导入配置的副本，原文件不变 |
| `settings.json` | 端口、模式、自启和节点选择 |
| `Data/` | Geo 数据、provider 文件和缓存 |
| `Sessions/` | 启动日志和状态；私密运行配置在内核加载后即删除，最多保留最近 5 次会话 |

TUN 会话和资源缓存位于 `/Library/Application Support/FlowBar/<uid>/`，由 root 管理。内核加载后即删除私密配置副本，会话结束后删除内核副本。配置文件权限为 0600；日志可由当前用户查看。TUN DNS 使用 SystemConfiguration 会话键，退出后自动撤销。

导入支持 YAML 锚点和合并键。provider 文件使用配置目录内的相对路径，拒绝越出目录、绝对路径和符号链接。运行配置统一使用应用设置的端口，关闭原 YAML 中的额外 listeners、旧端口、网页面板、远程控制入口及本地代理认证。

整份订阅配置目前需手动重新导入；尚无订阅管理器。

## 启动失败排查

从菜单打开日志查看实际原因。0.2.1 修复 macOS 管理员环境下 `nohup: can't detach from console` 导致的启动超时：改为原生独立会话启动，等待内核就绪后才报告成功，并保留授权和启动错误。

如果端口冲突，可在设置中更换端口。使用 TUN 前先退出其他代理客户端；配置有误时会在启动前通过 mihomo 检查。

## 验证

```sh
cargo test --locked -- --test-threads=1
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check

# 本地实际代理请求、DIRECT/REJECT、控制接口、配置检查
python3 scripts/smoke-test.py

# 使用已导入配置，完整启动/停止监督进程；不开启 TUN
dist/FlowBar.app/Contents/MacOS/FlowBar --engine-smoke

# 隔离配置和应用：原生窗口操作、节点选择和临时登录项注册/撤销
python3 scripts/ui-smoke-test.py

# 系统管理员授权、独立 root 监督进程、失败错误传递和停止清理
# 使用本地 DIRECT 测试配置，关闭 TUN 和 DNS 接管
python3 scripts/privileged-smoke-test.py
```

实际 TUN 联网和 DNS 接管需在目标环境中单独验证；管理员启动回归测试关闭这两项。登录项注册/撤销测试不代替真实重启后的登录测试。

## 代码结构与许可

`src/ui.rs` 为原生界面，`config.rs` 处理 YAML，`engine.rs` 协调连接，`supervisor.rs` 管理内核和临时 DNS，`process.rs` 管理子进程生命周期，`api.rs` 调用 mihomo 控制接口，`update.rs` 检查和安装更新。

FlowBar 源码采用 [MIT License](LICENSE)。独立的 mihomo 内核采用 GPL-3.0；构建时打包其许可和 Rust 依赖许可，详见 [第三方说明](Resources/THIRD_PARTY.md)。
