//! Build-time configuration of the rings-transport crate.

use std::env;

/// Emit `cfg(rings_transport_backend)` when a connection backend is compiled: the dummy
/// backend, native WebRTC, or browser WebRTC on a wasm target. Credit flow control sends only
/// through a backend, so its sending half exists exactly under this cfg.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(rings_transport_backend)");

    let enabled = |feature: &str| env::var_os(format!("CARGO_FEATURE_{feature}")).is_some();
    let target_is_wasm = env::var("CARGO_CFG_TARGET_FAMILY")
        .is_ok_and(|family| family.split(',').any(|family| family == "wasm"));
    if enabled("DUMMY") || enabled("NATIVE_WEBRTC") || (enabled("WEB_SYS_WEBRTC") && target_is_wasm)
    {
        println!("cargo:rustc-cfg=rings_transport_backend");
    }
}
