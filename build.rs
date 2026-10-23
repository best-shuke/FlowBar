//! Embeds the Windows icon, manifest and version information.
use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=Resources/windows");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let numbers: Vec<_> = version
        .split(['-', '+'])
        .next()
        .unwrap()
        .split('.')
        .collect();
    let macros = [
        format!("VERSION_MAJOR={}", numbers[0]),
        format!("VERSION_MINOR={}", numbers[1]),
        format!("VERSION_PATCH={}", numbers[2]),
        format!("VERSION_STRING=\"{version}\""),
    ];
    let directory =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("Resources/windows");
    let result = embed_resource::compile(
        directory.join("FlowBar.rc"),
        embed_resource::ParamsMacrosAndIncludeDirs(&macros, [&directory]),
    );
    // Cross-checking from another system has no resource compiler; real builds need one.
    if cfg!(windows) {
        result.manifest_required().unwrap();
    } else if let Err(error) = result.manifest_optional() {
        println!("cargo:warning=Windows resources not embedded: {error}");
    }
}
