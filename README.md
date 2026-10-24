# FlowBar

一个常驻菜单栏 / 托盘的代理客户端，用 Rust 和系统原生界面写成，支持 macOS、Windows 和 Linux。内核是 [rill](https://github.com/best-shuke/rill)：Rust 写的、与 [mihomo](https://github.com/MetaCubeX/mihomo) 配置兼容的代理内核。

[下载](https://github.com/best-shuke/FlowBar/releases/latest) · [使用说明](docs/guide.md) · [开发](docs/development.md)

很多 Clash 类客户端基于 Electron 或 WebView，功能很全，但也比较重。FlowBar 只做日常最常用的几件事：导入配置、连接、切节点。没有网页面板，也没有流量图，macOS 上界面空闲时只占 11 MB 左右内存（不含内核）。

## 功能

- 直接导入现成的 mihomo 配置，节点、策略组和规则原样使用
- TUN 模式接管全部流量，也可以只开本地 HTTP/SOCKS 代理
- 在菜单里切换规则 / 全局 / 直连模式和节点
- 开机自启，打开后自动连接
- IPv6 默认关闭，TUN 下也不会绕过代理，需要时在菜单里打开
- 应用内一键更新

暂时没有订阅管理，订阅更新后需要重新导入配置。

## 下载

**macOS / Linux**：在终端运行下面这行，会自动下载最新版、装好并打开。用这种方式装，macOS 不会拦截没有签名的应用。

```sh
curl -fsSL https://github.com/best-shuke/FlowBar/releases/latest/download/install.sh | sh
```

**Windows**：到 [Releases](https://github.com/best-shuke/FlowBar/releases/latest) 下载 `windows-x86_64-setup.exe`（ARM 电脑用 `windows-arm64-setup.exe`），双击安装，不需要管理员权限。也可以在 PowerShell 里运行：

```powershell
irm https://github.com/best-shuke/FlowBar/releases/latest/download/install.ps1 | iex
```

再次运行同样的命令或安装程序，就会更新到最新版。

### 压缩包

不想用脚本或安装程序的话，也可以下载压缩包，解压就能用，rill 内核已经打包在里面。

| 系统 | 安装包 |
| --- | --- |
| macOS 13 以上（Apple Silicon） | `macos-arm64.zip` |
| Windows 10（1803 以上） | `windows-x86_64.zip` / `windows-arm64.zip` |
| Linux（GTK 4.6 以上） | `linux-x86_64.tar.gz` / `linux-arm64.tar.gz` |

建议放在用户目录下，比如 `~/Applications` 或 `%LOCALAPPDATA%\Programs`，这样应用内更新才能替换自己。

- **macOS**：应用没有开发者签名，用浏览器下载的话，第一次打开会被系统拦下，去「系统设置 → 隐私与安全性」点「仍要打开」。
- **Windows**：SmartScreen 拦截时点「更多信息 → 仍要运行」，用浏览器下载的安装程序也一样。
- **Linux**：GNOME 要装 AppIndicator 扩展才能看到托盘图标，Ubuntu 默认已经装了。

## 快速上手

1. 打开 FlowBar，导入你的 YAML 配置。
2. 点「启动连接」。TUN 模式会请求一次管理员权限。
3. 在菜单栏或托盘菜单里切换模式和节点。

开 TUN 之前先退出其他代理客户端，避免路由冲突。连不上的话，先从菜单打开日志看看原因，常见问题见[使用说明](docs/guide.md#常见问题)。

## 从源码构建

需要 Rust 1.85 以上、Python 3，再准备一个 rill 可执行文件：

```sh
./scripts/build.sh /path/to/rill                       # macOS
./scripts/build-linux.sh /path/to/rill                 # Linux
.\scripts\build-windows.ps1 -Core C:\path\to\rill.exe  # Windows
```

各平台的依赖、测试和发布流程见[开发文档](docs/development.md)。

## 许可

[MIT](LICENSE)。打包的 rill 内核同样使用 MIT 许可，内置的地理数据库使用 GPL-3.0，详见[第三方说明](Resources/THIRD_PARTY.md)。
