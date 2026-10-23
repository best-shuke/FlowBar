# FlowBar

用 Rust 编写的轻量代理客户端，使用独立的 [mihomo](https://github.com/MetaCubeX/mihomo) 内核按 YAML 规则分流。三个平台都使用系统原生界面，无 WebView：

| 平台 | 界面 | 系统要求 | 安装包 |
| --- | --- | --- | --- |
| macOS | 菜单栏，objc2 调用 AppKit 和 ServiceManagement | macOS 13 及以上，Apple Silicon | `FlowBar-<版本>-macos-arm64.zip` |
| Windows | 系统托盘，原生 Win32 | Windows 10 1803 及以上，x86_64 / ARM64 | `FlowBar-<版本>-windows-<架构>.zip` |
| Linux | 系统托盘（StatusNotifierItem）和 GTK 4 窗口 | GTK 4.6 及以上，x86_64 / ARM64 | `FlowBar-<版本>-linux-<架构>.tar.gz` |

## 功能

- 导入完整的 mihomo YAML，保留节点、策略组、分流规则和 DNS 上游。
- TUN 接管应用流量；也可关闭 TUN，仅使用本地 HTTP/SOCKS 代理。
- 在菜单栏或托盘菜单中切换规则、全局、直连模式，以及策略组和节点。
- 登录时自动打开（macOS 登录项、Windows「Run」注册表项、Linux XDG autostart），并可独立设置打开应用后自动连接。
- 从 GitHub Releases 检查更新，在应用内下载、校验并替换后自动重启。
- 退出应用或主进程意外退出时，监督进程停止内核。
- 本地日志和配置检查，控制接口仅监听回环地址并使用随机密钥。

macOS 版界面冷启动空闲时，Apple Silicon 本机测得约 11 MB 内存 footprint，**不含 mihomo 内核**。总占用取决于配置、规则、流量和 TUN 协议栈。应用没有网页面板、实时流量图或日志订阅；仅在需要时查询策略组。

## 构建与安装

各平台都需要 Rust 1.85+、Python 3，以及适合本机系统和架构的 mihomo 可执行文件。内核会复制到安装包中，运行时不依赖其他客户端。仓库只保存源码、图标和许可；不包含 mihomo 二进制、个人配置、订阅、节点凭据或 Geo 数据。

### macOS

另需 Apple Command Line Tools。

```sh
git clone git@github.com:best-shuke/FlowBar.git
cd FlowBar
./scripts/build.sh /absolute/path/to/mihomo
mkdir -p ~/Applications
cp -R dist/FlowBar.app ~/Applications/
open ~/Applications/FlowBar.app
```

也可直接执行 `./scripts/build.sh`，脚本依次查找已安装的 FlowBar 内核、Clash Verge 内核或 PATH 中的 mihomo。构建脚本生成并验证本地 ad hoc 签名。

### Windows

另需 Visual Studio 生成工具（含 Windows SDK，用于嵌入图标和清单）。在 PowerShell 中：

```powershell
git clone git@github.com:best-shuke/FlowBar.git
cd FlowBar
.\scripts\build-windows.ps1 -Core C:\path\to\mihomo.exe
```

生成的 `dist\FlowBar` 目录可整体复制到用户可写的位置，例如 `%LOCALAPPDATA%\Programs\FlowBar`，然后运行其中的 `FlowBar.exe`。构建 ARM64 版时加上 `-Target aarch64-pc-windows-msvc`。

### Linux

另需 GTK 4 开发包：Debian/Ubuntu 为 `libgtk-4-dev`，Fedora 为 `gtk4-devel`。

```sh
git clone git@github.com:best-shuke/FlowBar.git
cd FlowBar
./scripts/build-linux.sh /absolute/path/to/mihomo
mkdir -p ~/.local/opt
cp -R dist/FlowBar ~/.local/opt/
~/.local/opt/FlowBar/flowbar
```

使用预编译包时，解压到同一位置即可：`tar -xzf FlowBar-<版本>-linux-x86_64.tar.gz -C ~/.local/opt`。如需在应用菜单中显示，可添加启动器：

```sh
cat > ~/.local/share/applications/flowbar.desktop <<EOF
[Desktop Entry]
Type=Application
Name=FlowBar
Exec=$HOME/.local/opt/FlowBar/flowbar
Icon=$HOME/.local/opt/FlowBar/flowbar.png
Categories=Network;
EOF
```

## 使用

1. 打开 FlowBar，从菜单或设置导入自己的 YAML，例如 `zgo-or.yaml`。
2. 使用 TUN 前，退出其他正在接管网络的代理客户端，避免路由冲突。
3. 点击「启动连接」。使用 TUN 时按系统提示授权管理员权限：macOS 为系统授权窗口，Windows 为 UAC，Linux 为 polkit。
4. 在菜单栏或托盘菜单中选择分流模式、策略组和节点；关闭设置窗口后仍在后台运行。

默认开启 TUN、关闭自动连接。关闭 TUN 时，本版不自动修改系统代理，需要在客户端中指定本地代理端口。TUN 当前没有常驻的管理员服务，登录后自动连接仍会请求管理员授权。

| 用途 | 默认地址 |
| --- | --- |
| HTTP / SOCKS | `127.0.0.1:17890` |
| 控制接口 | `127.0.0.1:19090` |
| DNS | `127.0.0.1:15353` |

端口可以在设置中修改。

### macOS

「登录时打开 FlowBar」使用 macOS 原生登录项；系统要求批准时会打开登录项设置。

### Windows

左键单击托盘图标打开设置，右键打开菜单。TUN 由经过 UAC 提升权限的监督进程启动内核；mihomo 内置 Wintun，无需另装驱动。

从浏览器下载的程序首次运行时，SmartScreen 可能提示「Windows 已保护你的电脑」，可点「更多信息 → 仍要运行」。应用内更新不受影响。

### Linux

托盘图标使用 StatusNotifierItem，KDE Plasma、Xfce 等桌面直接支持；GNOME 需要 AppIndicator 扩展（Ubuntu 默认已安装）。没有托盘时，设置窗口就是主界面，关闭窗口会退出 FlowBar 并停止连接。

TUN 通过 polkit 的 `pkexec` 获取管理员权限，DNS 由 TUN 的 DNS 劫持接管，不修改系统 DNS 设置。

## 检查更新

菜单或设置窗口中的「检查更新…」查询 GitHub 上最新的正式 Release。「自动检查更新」默认开启，应用打开约 10 秒后检查一次，之后每天检查一次；自动检查失败时不弹窗，下个小时重试。

发现新版本后，点击「安装并重启」会把本平台和架构的安装包下载到应用所在目录，校验大小和 GitHub 提供的 SHA-256，并确认新程序能在本机运行，然后替换当前版本，断开连接并重新打开。本地 HTTP 代理运行时，更新请求走该代理；TUN 运行时由 TUN 接管。应用需放在可写目录中；开发时直接运行的 `target/release/flowbar` 不能自我更新。命令行可用 `flowbar --check-update` 检查版本。

- **macOS**：另外校验代码签名、Bundle ID 和版本号，再原子替换 `FlowBar.app`。签名仍是 ad hoc，安装包的可信度来自 GitHub 的 HTTPS 和 Release 本身，没有开发者身份校验。应用内下载不会带隔离标记，所以更新后不会被 Gatekeeper 拦截。但第一次用浏览器下载 Release 安装时会被拦截，可在「系统设置 → 隐私与安全性」中允许，或改用终端下载：

  ```sh
  curl -fLO https://github.com/best-shuke/FlowBar/releases/latest/download/FlowBar-<版本>-macos-arm64.zip
  ```

- **Windows**：运行中的程序目录不能改名，所以逐个替换其中的文件；旧文件先移到临时目录，新版本启动后删除。
- **Linux**：原子交换整个 `FlowBar` 目录。

### 发布新版本

仓库须为公开，应用才能匿名读取 Release。发布步骤：

1. 同时修改 `Cargo.toml` 的 `version`、`Resources/Info.plist` 的 `CFBundleShortVersionString`（并递增 `CFBundleVersion`），执行 `cargo build` 更新 `Cargo.lock`；`cargo test` 会检查前两者一致。
2. 提交后打标签并推送：`git tag v0.3.0 && git push origin main v0.3.0`。
3. `.github/workflows/release.yml` 核对标签与版本，下载并校验固定版本的 mihomo（`MIHOMO_VERSION`），运行测试，在各平台的 runner 上构建 macOS arm64、Windows x86_64 / ARM64 和 Linux x86_64 / ARM64 安装包并发布 Release。

升级内置 mihomo 时，同时修改 `release.yml` 和 `ci.yml` 中的 `MIHOMO_VERSION`，以及各平台的 `sha256`。

## 配置与数据

| 平台 | 用户数据 | TUN 会话和资源缓存 |
| --- | --- | --- |
| macOS | `~/Library/Application Support/FlowBar/` | `/Library/Application Support/FlowBar/<uid>/`，由 root 管理 |
| Linux | `~/.local/share/flowbar/` | `/var/lib/flowbar/<uid>/`，由 root 管理 |
| Windows | `%LOCALAPPDATA%\FlowBar\` | 与用户数据相同，提升权限的进程以同一用户运行 |

| 路径 | 内容 |
| --- | --- |
| `profile.yaml` | 导入配置的副本，原文件不变 |
| `settings.json` | 端口、模式、自启和节点选择 |
| `Data/` | Geo 数据、provider 文件和缓存 |
| `Sessions/` | 启动日志和状态；私密运行配置在内核加载后即删除，最多保留最近 5 次会话 |

macOS 和 Linux 的 TUN 内核由 root 从自己的目录运行：内核加载后即删除私密配置副本，会话结束后删除内核副本。配置文件权限为 0600；日志可由当前用户查看。macOS 的 TUN DNS 使用 SystemConfiguration 会话键，退出后自动撤销。

导入支持 YAML 锚点和合并键。provider 文件使用配置目录内的相对路径，拒绝越出目录、绝对路径和符号链接。运行配置统一使用应用设置的端口，关闭原 YAML 中的额外 listeners、旧端口、网页面板、远程控制入口及本地代理认证。

整份订阅配置目前需手动重新导入；尚无订阅管理器。

## 启动失败排查

从菜单打开日志查看实际原因。启动时以独立会话运行监督进程，等待内核就绪后才报告成功，并保留授权和启动错误。

如果端口冲突，可在设置中更换端口。使用 TUN 前先退出其他代理客户端；配置有误时会在启动前通过 mihomo 检查。

## 验证

`.github/workflows/ci.yml` 在每次推送和 Pull Request 时于 macOS、Windows 和 Linux 上运行格式检查、Clippy、单元测试、打包和界面冒烟测试。本地可运行：

```sh
cargo test --locked -- --test-threads=1
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check

# 本地实际代理请求、DIRECT/REJECT、控制接口、配置检查
python3 scripts/smoke-test.py

# 使用已导入配置，完整启动/停止监督进程；不开启 TUN
dist/FlowBar.app/Contents/MacOS/FlowBar --engine-smoke

# 隔离配置和应用：原生窗口操作、节点选择和临时登录项注册/撤销
# Linux 需要显示器和 D-Bus 会话，例如在前面加 xvfb-run -a dbus-run-session --
python3 scripts/ui-smoke-test.py

# Linux 托盘：模拟桌面面板，通过 D-Bus 点击托盘菜单
xvfb-run -a dbus-run-session -- python3 scripts/tray-smoke-test.py

# macOS 和 Linux：系统管理员授权、独立 root 监督进程、失败错误传递和停止清理
# 使用本地 DIRECT 测试配置，关闭 TUN 和 DNS 接管；Linux 可用 FLOWBAR_AUTHORIZE=sudo 代替 pkexec
python3 scripts/privileged-smoke-test.py
```

实际 TUN 联网和 DNS 接管需在目标环境中单独验证；管理员启动回归测试关闭这两项。登录项注册/撤销测试不代替真实重启后的登录测试。

## 代码结构与许可

`src/ui/mod.rs` 是三个平台共用的界面状态、菜单和操作；`ui/macos.rs`、`ui/windows.rs` 和 `ui/linux.rs` 分别是 AppKit、Win32 和 GTK 4 界面，`ui/icon.rs` 绘制 Windows 和 Linux 的托盘图标。`config.rs` 处理 YAML，`engine.rs` 协调连接和管理员授权，`supervisor.rs` 管理内核和临时 DNS，`process.rs` 管理子进程生命周期，`api.rs` 调用 mihomo 控制接口，`update.rs` 检查和安装更新。

FlowBar 源码采用 [MIT License](LICENSE)。独立的 mihomo 内核采用 GPL-3.0；构建时打包其许可和 Rust 依赖许可，详见 [第三方说明](Resources/THIRD_PARTY.md)。
