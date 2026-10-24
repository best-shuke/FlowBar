FlowBar is a Rust native controller for the rill proxy core: AppKit and ServiceManagement on
macOS, Win32 on Windows, and GTK 4 with a StatusNotifierItem on Linux. It starts rill as a
separate process. No Swift code or WebView is used by this build.

rill: https://github.com/best-shuke/rill (MIT), a Rust proxy core that reads mihomo
configurations, derived from meow-rs (https://github.com/meow-rs/meow-rs, MIT).
Release builds bundle an unmodified rill release binary; the release notes name its version.
The bundled core's exact version can be checked with `rill -v`
(Contents/Resources/rill on macOS, next to the FlowBar executable elsewhere).
LICENSE-RILL and NOTICE-RILL contain its license and origin. The notices of the code it is
built from — Rust crates, BoringSSL, lwIP and, on Windows, the embedded Wintun driver
(LICENSE-WINTUN.txt) — are in LICENSE-RILL-THIRD-PARTY.txt.

The file named `mihomo` (`mihomo.exe` on Windows) is not a program and is never run: it is a
text placeholder that lets FlowBar 0.4.0 and earlier, which bundled mihomo, update to this
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
