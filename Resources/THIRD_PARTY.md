FlowBar is a Rust native controller for mihomo: AppKit and ServiceManagement on macOS,
Win32 on Windows, and GTK 4 with a StatusNotifierItem on Linux. It starts mihomo as a
separate process. No Swift code or WebView is used by this build.

mihomo: https://github.com/MetaCubeX/mihomo (GPL-3.0).
Release builds bundle an unmodified mihomo release binary; the release notes name its version.
A local build may instead copy the user's already installed mihomo executable, for example
from Clash Verge. It is copied into the package, so Clash Verge is not needed at runtime.
The bundled core's exact version can be checked with `mihomo -v`
(Contents/Resources/mihomo on macOS, next to the FlowBar executable elsewhere).
LICENSE-MIHOMO contains its license. No modifications are made to the core's source.

On Linux, FlowBar uses the system's GTK 4 libraries (LGPL-2.1-or-later) through dynamic
linking; they are not included in the package.

Rust dependency notices and complete available license files are bundled in
LICENSE-RUST-DEPENDENCIES.txt, generated from the locked Cargo dependency graph.
