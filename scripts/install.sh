#!/bin/sh
# Installs or updates FlowBar from the latest release, for the current user:
#   curl -fsSL https://github.com/best-shuke/FlowBar/releases/latest/download/install.sh | sh
# macOS gets ~/Applications/FlowBar.app; Linux gets ~/.local/opt/FlowBar and a launcher.
# Downloads with curl carry no quarantine attribute, so Gatekeeper lets the app open.
set -eu

REPOSITORY=best-shuke/FlowBar
APP_ID=dev.local.FlowBar

fail() {
    echo "错误：$*" >&2
    exit 1
}

# Quits a running FlowBar, so the new one starts instead of forwarding to it. Its
# supervisor stops the core once FlowBar is gone.
quit_running() {
    pkill -u "$(id -u)" -x "$1" 2>/dev/null || return 0
    i=0
    while pgrep -u "$(id -u)" -x "$1" >/dev/null 2>&1 && [ $i -lt 100 ]; do
        sleep 0.1
        i=$((i + 1))
    done
}

# Escapes a path for the Exec key of a desktop entry, quoted.
desktop_exec() {
    printf '"%s"' "$(printf '%s' "$1" | sed -e 's/[\\"`$]/\\\\&/g' -e 's/%/%%/g')"
}

main() {
    [ "$(id -u)" -ne 0 ] || fail "请不要用 sudo 运行，FlowBar 安装在当前用户的目录里。"
    command -v curl >/dev/null || fail "需要 curl。"

    case "$(uname -s)" in
    Darwin)
        system=macos
        # uname reports x86_64 under Rosetta.
        [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ] ||
            fail "FlowBar 目前只支持 Apple Silicon 的 Mac。"
        major="$(sw_vers -productVersion | cut -d. -f1)"
        [ "$major" -ge 13 ] || fail "FlowBar 需要 macOS 13 或更高版本。"
        arch=arm64
        extension=zip
        ;;
    Linux)
        system=linux
        case "$(uname -m)" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=arm64 ;;
        *) fail "不支持的架构：$(uname -m)" ;;
        esac
        extension=tar.gz
        ;;
    *) fail "这个脚本只用于 macOS 和 Linux，Windows 请看 https://github.com/$REPOSITORY#下载" ;;
    esac

    # The latest release page redirects to its tag, without the API's rate limit.
    latest="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPOSITORY/releases/latest")" ||
        fail "无法连接 GitHub。"
    version="${latest##*/v}"
    case "$version" in
    [0-9]*) ;;
    *) fail "找不到已发布的版本。" ;;
    esac
    asset="FlowBar-$version-$system-$arch.$extension"

    temporary="$(mktemp -d)"
    trap 'rm -rf "$temporary"' EXIT
    echo "正在下载 FlowBar $version（$system-$arch）…"
    curl -fL --progress-bar -o "$temporary/$asset" \
        "https://github.com/$REPOSITORY/releases/download/v$version/$asset" ||
        fail "下载失败。"

    if [ $system = macos ]; then
        ditto -x -k "$temporary/$asset" "$temporary"
        new="$temporary/FlowBar.app"
        codesign --verify --deep --strict "$new" || fail "安装包签名校验失败。"
        "$new/Contents/MacOS/FlowBar" --version >/dev/null || fail "FlowBar 无法在这台电脑上运行。"
        target="$HOME/Applications/FlowBar.app"
        # Updates a copy already in /Applications instead of adding a second one.
        if [ ! -e "$target" ] && [ -d /Applications/FlowBar.app ] && [ -w /Applications ]; then
            target=/Applications/FlowBar.app
        fi
        quit_running FlowBar
    else
        tar -xzf "$temporary/$asset" -C "$temporary"
        new="$temporary/FlowBar"
        if ! "$new/flowbar" --version >/dev/null; then
            fail "FlowBar 无法运行，可能缺少 GTK 4.6 以上的运行库。
Debian/Ubuntu：sudo apt install libgtk-4-1
Fedora：sudo dnf install gtk4
Arch：sudo pacman -S gtk4"
        fi
        target="$HOME/.local/opt/FlowBar"
        quit_running flowbar
    fi

    mkdir -p "$(dirname "$target")"
    rm -rf "$target"
    mv "$new" "$target"

    if [ $system = macos ]; then
        echo "已安装到 $target"
        open "$target"
    else
        applications="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
        mkdir -p "$applications"
        cat >"$applications/$APP_ID.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=FlowBar
Comment=mihomo 代理托盘
Exec=$(desktop_exec "$target/flowbar")
Icon=$target/flowbar.png
Terminal=false
Categories=Network;
EOF
        echo "已安装到 $target，应用菜单里也能找到 FlowBar。"
        if [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
            nohup "$target/flowbar" >/dev/null 2>&1 </dev/null &
        fi
    fi
}

# Runs only once the whole script has arrived through the pipe.
main "$@"
