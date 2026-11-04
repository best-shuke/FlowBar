FlowBar is a Rust native controller for mihomo: AppKit and ServiceManagement on macOS,
Win32 on Windows, and GTK 4 with a StatusNotifierItem on Linux. It starts mihomo as a
separate process. No Swift code or WebView is used by this build.

mihomo: https://github.com/MetaCubeX/mihomo (GPL-3.0, whose license text is LICENSE-GPL-3.0).
Release builds bundle an unmodified mihomo release binary; the release notes name its version
and link its source. A local build may instead copy the user's already installed mihomo
executable, for example from Clash Verge. It is copied into the package, so Clash Verge is not
needed at runtime. The bundled core's exact version can be checked with `mihomo -v`
(Contents/Resources/mihomo on macOS, next to the FlowBar executable elsewhere).
No modifications are made to the core's source.

The file named `rill` (`rill.exe` on Windows) is not a program and is never run: it is a text
placeholder that lets FlowBar 0.5.0 to 0.8.2, which bundled the rill core, update to this
package.

meta-rules-dat: https://github.com/MetaCubeX/meta-rules-dat (GPL-3.0, whose license text is
LICENSE-GPL-3.0). Release builds bundle its geoip.metadb and geosite.dat (as GeoSite.dat),
unmodified, from the release published on the day of the build. The IP data in geoip.metadb
comes from https://github.com/Loyalsoldier/geoip under CC BY-SA 4.0
(https://creativecommons.org/licenses/by-sa/4.0/) and includes GeoLite2 data created by
MaxMind, available from https://www.maxmind.com. Databases FlowBar downloads later come from
the same project or from the profile's geox-url.

On Linux, FlowBar uses the system's GTK 4 libraries (LGPL-2.1-or-later) through dynamic
linking; they are not included in the package.

Rust dependency notices and complete available license files are bundled in
LICENSE-RUST-DEPENDENCIES.txt, generated from the locked Cargo dependency graph.
