#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]

//! Builds the strict Simulator mode interposer (`ios/Strict/zios_strict.c`) as a
//! dylib and links it into Simulator builds that enable the `strict` feature.
//!
//! dyld applies `__interpose` tuples from libraries loaded at launch, so linking
//! the dylib (rather than relying on `DYLD_INSERT_LIBRARIES`) keeps strict mode on
//! however the app is launched. `scripts/build_app.sh` copies the dylib from this
//! build script's `OUT_DIR` into `ZiOS.app/Frameworks`.

use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let source = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default())
        .join("../../ios/Strict/zios_strict.c");
    println!("cargo::rerun-if-changed={}", source.display());

    let strict = env::var_os("CARGO_FEATURE_STRICT").is_some();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let target_abi = env::var("CARGO_CFG_TARGET_ABI").unwrap_or_default();
    let simulator = target_os == "ios" && (target_env == "sim" || target_abi == "sim");
    if !strict || !simulator {
        return;
    }
    // A non-Apple host can only type-check; there is nothing to link.
    if !cfg!(target_os = "macos") {
        println!("cargo::warning=strict interposer skipped: this host cannot build iOS dylibs");
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let dylib = out_dir.join("libzios_strict.dylib");
    let deployment_target = env::var("IPHONEOS_DEPLOYMENT_TARGET").unwrap_or_else(|_| "17.0".into());
    let output = Command::new("xcrun")
        .args(["-sdk", "iphonesimulator", "clang"])
        .arg("-target")
        .arg(format!("arm64-apple-ios{deployment_target}-simulator"))
        .args([
            "-dynamiclib",
            "-O2",
            "-g",
            "-Wall",
            "-Wno-unused-parameter",
            "-Werror",
            "-install_name",
            "@executable_path/Frameworks/libzios_strict.dylib",
            "-o",
        ])
        .arg(&dylib)
        .arg(&source)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            println!(
                "cargo::error=building the strict interposer failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        Err(error) => {
            println!("cargo::error=running xcrun failed: {error}");
            return;
        }
    }
    println!("cargo::rustc-link-search=native={}", out_dir.display());
    println!("cargo::rustc-link-lib=dylib=zios_strict");
}
