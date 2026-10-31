#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]

//! Links ios_system.framework from `ios/Frameworks` (see
//! scripts/fetch_ios_system.sh). When the frameworks are absent (for example, a
//! Linux type-check), `ios_system_missing` is set and the crate's functions
//! report that commands are unavailable instead of failing to link.

use std::{env, path::PathBuf};

fn main() {
    println!("cargo::rustc-check-cfg=cfg(ios_system_missing)");
    println!("cargo::rerun-if-env-changed=ZIOS_FRAMEWORKS_DIR");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "ios" {
        println!("cargo::rustc-cfg=ios_system_missing");
        return;
    }
    let simulator = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("sim")
        || env::var("CARGO_CFG_TARGET_ABI").as_deref() == Ok("sim");
    let frameworks = env::var_os("ZIOS_FRAMEWORKS_DIR").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join("../../ios/Frameworks")
    });
    let xcframework = frameworks.join("ios_system.xcframework");
    println!("cargo::rerun-if-changed={}", xcframework.display());
    let slice = std::fs::read_dir(&xcframework).ok().and_then(|entries| {
        entries.filter_map(Result::ok).map(|entry| entry.path()).find(|path| {
            let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            name.starts_with("ios-arm64") && name.contains("simulator") == simulator
        })
    });
    let Some(slice) = slice else {
        println!(
            "cargo::warning=ios_system.xcframework not found in {}; run scripts/fetch_ios_system.sh. Shell commands will be unavailable.",
            frameworks.display()
        );
        println!("cargo::rustc-cfg=ios_system_missing");
        return;
    };
    println!("cargo::rustc-link-search=framework={}", slice.display());
    println!("cargo::rustc-link-lib=framework=ios_system");
}
