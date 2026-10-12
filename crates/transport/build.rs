//! Build-time configuration of the rings-transport crate.

use std::env;

/// Emit the cfg aliases of the crate.
///
/// - `cfg(rings_transport_backend)` when a connection backend is compiled: the dummy backend,
///   native WebRTC, or browser WebRTC on a wasm target. Credit flow control sends only through a
///   backend, so its sending half exists exactly under this cfg.
/// - `cfg(rings_transport_runtime)` when an executor exists to spawn the inbound tasks of a
///   connection callback, its per-lane drainers and its invalid-frame report worker: tokio off
///   wasm, or the browser executor of `web-sys-webrtc` on wasm. It aliases
///   `any(all(not(target_family = "wasm"), feature = "tokio"),
///   all(target_family = "wasm", feature = "web-sys-webrtc"))`.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(rings_transport_backend)");
    println!("cargo:rustc-check-cfg=cfg(rings_transport_runtime)");

    let enabled = |feature: &str| env::var_os(format!("CARGO_FEATURE_{feature}")).is_some();
    let target_is_wasm = env::var("CARGO_CFG_TARGET_FAMILY")
        .is_ok_and(|family| family.split(',').any(|family| family == "wasm"));
    if enabled("DUMMY") || enabled("NATIVE_WEBRTC") || (enabled("WEB_SYS_WEBRTC") && target_is_wasm)
    {
        println!("cargo:rustc-cfg=rings_transport_backend");
    }
    if (enabled("TOKIO") && !target_is_wasm) || (enabled("WEB_SYS_WEBRTC") && target_is_wasm) {
        println!("cargo:rustc-cfg=rings_transport_runtime");
    }
}
