# 使用说明

## 安装

从 [Releases](https://github.com/best-shuke/FlowBar/releases/latest) 下载对应平台的安装包。FlowBar 要放在自己有写权限的目录里，应用内更新才能替换它。

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

1. 打开 FlowBar，从菜单或设置里导入 YAML 配置。支持 YAML 锚点和合并键。
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
