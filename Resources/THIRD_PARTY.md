FlowBar is a Rust native macOS controller using AppKit and ServiceManagement.
It starts mihomo as a separate process. No Swift code or WebView is used by this build.

mihomo: https://github.com/MetaCubeX/mihomo (GPL-3.0).
The local build may copy the user's already installed mihomo executable from Clash Verge.
It is copied into the app bundle, so Clash Verge is not needed at runtime.
The copied core's exact version can be checked with Contents/Resources/mihomo -v.
LICENSE-MIHOMO contains its license. No modifications are made to the core's source.

Rust dependency notices and complete available license files are bundled in
LICENSE-RUST-DEPENDENCIES.txt, generated from the locked Cargo dependency graph.
