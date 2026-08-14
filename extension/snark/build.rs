//! Build-time platform cfgs for the SNARK extension crate.

use std::env;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(rings_native)");
    println!("cargo:rustc-check-cfg=cfg(rings_browser)");

    let target_is_wasm =
        env::var("CARGO_CFG_TARGET_FAMILY").is_ok_and(|target_family| target_family == "wasm");
    let browser_enabled = env::var_os("CARGO_FEATURE_BROWSER").is_some();
    let node_enabled = env::var_os("CARGO_FEATURE_NODE").is_some();

    if target_is_wasm && browser_enabled {
        println!("cargo:rustc-cfg=rings_browser");
    } else if !target_is_wasm && node_enabled {
        println!("cargo:rustc-cfg=rings_native");
    }
}
