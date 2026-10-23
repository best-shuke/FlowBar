# 开发

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

## 测试

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

## 发布

1. 修改 `Cargo.toml` 的 `version` 和 `Resources/Info.plist` 的 `CFBundleShortVersionString`，两者必须一致（`cargo test` 会检查）；`CFBundleVersion` 加一。然后运行 `cargo build` 更新 `Cargo.lock`。
2. 提交并推送，等 CI 通过。
3. 打标签并推送：`git tag v0.3.0 && git push origin v0.3.0`。

推送标签后，`.github/workflows/release.yml` 会核对版本号，下载并校验固定版本的 mihomo，跑一遍测试，然后构建 5 个安装包（macOS arm64、Windows x86_64 / ARM64、Linux x86_64 / ARM64）并发布 Release。仓库必须是公开的，应用才能读到 Release。

升级内置的 mihomo 时，要同时修改 `release.yml` 和 `ci.yml` 里的 `MIHOMO_VERSION` 和各平台的 `sha256`。

## 代码结构

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
