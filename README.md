# FlowBar

一个用 Rust 写的 [mihomo](https://github.com/MetaCubeX/mihomo) 桌面客户端。导入 mihomo 的 YAML 配置，就能在菜单栏或托盘里开关代理、切换节点。界面用的都是系统原生控件，没有 WebView，也没有网页面板。

| 平台 | 界面 | 系统要求 |
| --- | --- | --- |
| macOS | 菜单栏（AppKit） | macOS 13 以上，Apple Silicon |
| Windows | 系统托盘（Win32） | Windows 10 1803 以上，x86_64 / ARM64 |
| Linux | 系统托盘 + GTK 4 窗口 | GTK 4.6 以上，x86_64 / ARM64 |

## 功能

- 直接导入完整的 mihomo 配置，节点、策略组、规则和 DNS 原样使用，支持 YAML 锚点和合并键
- TUN 模式，或者只开本地 HTTP/SOCKS 代理
- 在菜单里切换规则 / 全局 / 直连模式和各策略组的节点
- 登录时自动打开，可以单独设置打开后自动连接
- 应用内检查更新，一键安装并重启
- FlowBar 退出或崩溃时内核会跟着停掉，不会留在后台

macOS 版界面进程空闲时大约占 11 MB 内存，不算 mihomo 内核。

## 安装

从 [Releases](https://github.com/best-shuke/FlowBar/releases/latest) 下载对应平台的安装包，mihomo 内核已经打包在里面。FlowBar 要放在自己有写权限的目录里，应用内更新才能替换它。

**macOS**：解压后把 `FlowBar.app` 放进 `~/Applications`。应用只有 ad hoc 签名，用浏览器下载的话，第一次打开会被 Gatekeeper 拦下，到「系统设置 → 隐私与安全性」里允许一下就好。用终端下载不会被拦：

```sh
curl -fLO https://github.com/best-shuke/FlowBar/releases/latest/download/FlowBar-<版本>-macos-arm64.zip
```

**Windows**：解压到比如 `%LOCALAPPDATA%\Programs\FlowBar`，运行里面的 `FlowBar.exe`。如果 SmartScreen 提示「Windows 已保护你的电脑」，点「更多信息 → 仍要运行」。

**Linux**：

```sh
mkdir -p ~/.local/opt
tar -xzf FlowBar-<版本>-linux-x86_64.tar.gz -C ~/.local/opt
~/.local/opt/FlowBar/flowbar
```

想让它出现在应用菜单里，可以加一个启动器：

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

1. 打开 FlowBar，从菜单或设置里导入 YAML 配置。
2. 点「启动连接」。TUN 模式要管理员权限，会弹出系统授权：macOS 是授权窗口，Windows 是 UAC，Linux 是 polkit。
3. 连上以后在菜单里选模式、策略组和节点。关掉设置窗口后 FlowBar 仍在后台运行。

另外几点：

- 开 TUN 前先退出其他代理客户端（比如 Clash Verge），否则路由会冲突。
- TUN 默认开启。关掉 TUN 后 FlowBar 不会改系统代理，需要在各个程序里自己填代理地址。
- 现在还没有常驻的提权服务，所以登录后自动连接 TUN 时也会弹授权。
- 订阅更新后要重新导入配置，暂时没有订阅管理。

默认端口如下，可以在设置里改：

| 用途 | 地址 |
| --- | --- |
| HTTP / SOCKS | `127.0.0.1:17890` |
| 控制接口 | `127.0.0.1:19090` |
| DNS | `127.0.0.1:15353` |

**macOS**：登录时打开用的是系统登录项，第一次开启时系统可能会让你去登录项设置里批准。TUN 模式改的系统 DNS 在断开后会自动恢复。

**Windows**：左键点托盘图标打开设置，右键打开菜单。TUN 用的是 mihomo 自带的 Wintun，不用另装驱动。

**Linux**：托盘基于 StatusNotifierItem，KDE Plasma 和 Xfce 直接能用，GNOME 需要 AppIndicator 扩展（Ubuntu 默认装了）。桌面没有托盘时，设置窗口就是主界面，关掉窗口会退出 FlowBar 并断开连接。TUN 通过 `pkexec` 提权，DNS 由 TUN 劫持，不会改系统的 DNS 设置。

## 更新

「自动检查更新」默认开启：打开 FlowBar 约 10 秒后查一次 GitHub Releases，之后每天查一次。自动检查失败时不会弹窗，一小时后再试。也可以在菜单里点「检查更新…」，或者在命令行运行 `flowbar --check-update`。

有新版本时点「安装并重启」。FlowBar 会下载本平台的安装包，核对大小和 SHA-256，确认新版本能在这台机器上运行，然后替换旧版本，断开连接并重新打开。macOS 上还会核对签名、Bundle ID 和版本号。连接开着的时候，下载会走代理。

直接运行 `target/release/flowbar` 时只能检查更新，不能安装。

仓库是私有的，或者还没发过 Release 时，菜单里会显示「更新源不可用」。GitHub 对这两种情况都返回 404，FlowBar 分不出是哪一种。私有部署的话，关掉「自动检查更新」，自己拉代码重新构建就行。

## 数据存放位置

| 平台 | 用户数据 | TUN 运行目录（root 管理） |
| --- | --- | --- |
| macOS | `~/Library/Application Support/FlowBar/` | `/Library/Application Support/FlowBar/<uid>/` |
| Linux | `~/.local/share/flowbar/` | `/var/lib/flowbar/<uid>/` |
| Windows | `%LOCALAPPDATA%\FlowBar\` | 和用户数据相同 |

用户数据目录里有：

| 路径 | 内容 |
| --- | --- |
| `profile.yaml` | 导入配置的副本，原文件不会被改动 |
| `settings.json` | 端口、模式、自启和节点选择 |
| `Data/` | Geo 数据库、provider 文件和缓存 |
| `Sessions/` | 每次连接的日志和状态，只保留最近 5 次 |

## 安全

- 控制接口只监听 `127.0.0.1`，每次连接都会生成新的随机密钥。
- 运行时 FlowBar 会统一改写端口，并去掉原配置里额外的 listeners、网页面板、远程控制入口和本地代理认证。
- 运行配置里有节点密码，文件权限是 0600，内核读完就删。macOS 和 Linux 上，TUN 内核由 root 在单独的目录里运行，断开后连内核副本一起删除。
- provider 文件只能用配置目录里的相对路径，绝对路径、跳出目录的路径和符号链接都会被拒绝。
- 安装包只有 ad hoc 签名，没有开发者身份，可信度来自 GitHub 的 HTTPS 和 Release 本身。

## 常见问题

**连接失败怎么办？**

先从菜单打开日志看具体原因。端口被占用就到设置里换一个。配置写错的话，mihomo 在启动前的配置检查阶段就会报出来。

**一直显示「正在下载地理数据库」，或者提示缺少数据库**

配置里用到 `GEOIP`、`GEOSITE` 或 `IP-ASN` 规则时，mihomo 第一次检查配置就会去下载数据库。下载地址取自配置里的 `geox-url`，没写就用 mihomo 的默认地址；检查过程不会读取终端里设置的代理环境变量。

下载不下来的话，先停止连接，把其他客户端里现成的数据库复制到用户数据目录的 `Data/` 下，再重新连接。也可以把数据库和 YAML 放在同一个目录，然后重新导入。文件名要对得上：

| 规则 | 数据库文件 |
| --- | --- |
| `GEOIP` | `Country.mmdb`、`geoip.metadb` 或 `geoip.db`；配置了 `geodata-mode: true` 时用 `GeoIP.dat` |
| `GEOSITE` | `GeoSite.dat` |
| `IP-ASN` | `ASN.mmdb` |

mmdb 和 dat 是两种格式，改文件名不能互换。配置检查的完整输出在 `Sessions/<会话 ID>/validation.log`。

## 从源码构建

需要 Rust 1.85 以上、Python 3，以及一个和本机系统、架构对应的 mihomo 可执行文件，构建时会把它打进安装包。仓库里不包含 mihomo 二进制和 Geo 数据。

**macOS**（另需 Xcode Command Line Tools）：

```sh
git clone https://github.com/best-shuke/FlowBar.git
cd FlowBar
./scripts/build.sh /path/to/mihomo
cp -R dist/FlowBar.app ~/Applications/
```

不传路径时，脚本会依次查找已安装的 FlowBar 自带的内核、Clash Verge 的内核和 PATH 里的 mihomo。

**Windows**（另需带 Windows SDK 的 Visual Studio 生成工具），在 PowerShell 里：

```powershell
git clone https://github.com/best-shuke/FlowBar.git
cd FlowBar
.\scripts\build-windows.ps1 -Core C:\path\to\mihomo.exe
```

生成的程序在 `dist\FlowBar`。构建 ARM64 版加上 `-Target aarch64-pc-windows-msvc`。

**Linux**（另需 GTK 4 开发包，Debian/Ubuntu 是 `libgtk-4-dev`，Fedora 是 `gtk4-devel`）：

```sh
git clone https://github.com/best-shuke/FlowBar.git
cd FlowBar
./scripts/build-linux.sh /path/to/mihomo
cp -R dist/FlowBar ~/.local/opt/
```

## 开发

### 测试

CI 会在 macOS、Windows 和 Linux 上跑格式检查、Clippy、单元测试、打包和界面冒烟测试。本地可以这样跑：

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked -- --test-threads=1

# 代理请求、DIRECT/REJECT、控制接口、配置检查
python3 scripts/smoke-test.py

# 窗口操作、节点选择、登录项注册和撤销
# Linux 需要显示器和 D-Bus，前面加上 xvfb-run -a dbus-run-session --
python3 scripts/ui-smoke-test.py

# Linux 托盘菜单
xvfb-run -a dbus-run-session -- python3 scripts/tray-smoke-test.py

# macOS 和 Linux 的提权启动、错误传递和停止清理
# Linux 可以设置 FLOWBAR_AUTHORIZE=sudo 来代替 pkexec
python3 scripts/privileged-smoke-test.py

# 用已导入的配置完整启动、停止一次
dist/FlowBar.app/Contents/MacOS/FlowBar --engine-smoke
```

这些测试都不开 TUN 和 DNS 接管，TUN 实际能不能联网还得在真实环境里试。登录项测试也代替不了真的重启后登录。

### 发布

1. 修改 `Cargo.toml` 的 `version` 和 `Resources/Info.plist` 的 `CFBundleShortVersionString`，两者必须一致（`cargo test` 会检查）；`CFBundleVersion` 加一。然后运行 `cargo build` 更新 `Cargo.lock`。
2. 提交并推送，等 CI 通过。
3. 打标签并推送：`git tag v0.3.0 && git push origin v0.3.0`。

推送标签后，`.github/workflows/release.yml` 会核对版本号，下载并校验固定版本的 mihomo，跑一遍测试，然后构建 5 个安装包（macOS arm64、Windows x86_64 / ARM64、Linux x86_64 / ARM64）并发布 Release。仓库必须是公开的，应用才能读到 Release。

升级内置的 mihomo 时，要同时修改 `release.yml` 和 `ci.yml` 里的 `MIHOMO_VERSION` 和各平台的 `sha256`。

### 代码结构

| 文件 | 作用 |
| --- | --- |
| `src/config.rs` | 导入 YAML，生成运行配置 |
| `src/engine.rs` | 连接流程和管理员授权 |
| `src/supervisor.rs` | 监督内核运行，管理临时 DNS |
| `src/process.rs` | 子进程生命周期 |
| `src/api.rs` | 调用 mihomo 控制接口 |
| `src/update.rs` | 检查和安装更新 |
| `src/ui/mod.rs` | 三个平台共用的界面状态、菜单和操作 |
| `src/ui/macos.rs`、`windows.rs`、`linux.rs` | AppKit、Win32、GTK 4 界面 |
| `src/ui/icon.rs` | 绘制 Windows 和 Linux 的托盘图标 |

## 许可

FlowBar 使用 [MIT 许可](LICENSE)。打包的 mihomo 内核使用 GPL-3.0，它和 Rust 依赖的许可都会一起打包，详见[第三方说明](Resources/THIRD_PARTY.md)。
