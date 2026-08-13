//! Build-time metadata generation for the rings-node crate.

#[cfg(feature = "ffi")]
extern crate cbindgen;
use std::env;
use std::process::Command;

fn gen_version() {
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    {
        let Ok(git_short_hash) = String::from_utf8(output.stdout) else {
            return;
        };
        println!("cargo:rustc-env=GIT_SHORT_HASH={git_short_hash}");
    }
}

fn emit_platform_cfg() {
    println!("cargo:rustc-check-cfg=cfg(rings_native)");
    println!("cargo:rustc-check-cfg=cfg(rings_browser)");

    let target_is_wasm = env::var("CARGO_CFG_TARGET_FAMILY")
        .is_ok_and(|family| family.split(',').any(|family| family == "wasm"));
    let browser_enabled = env::var_os("CARGO_FEATURE_BROWSER").is_some();
    let node_enabled = env::var_os("CARGO_FEATURE_NODE").is_some();
    if target_is_wasm && browser_enabled {
        println!("cargo:rustc-cfg=rings_browser");
    } else if !target_is_wasm && node_enabled {
        println!("cargo:rustc-cfg=rings_native");
    }
}

fn main() {
    emit_platform_cfg();
    gen_version();
}
