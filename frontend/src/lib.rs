//! Rings browser frontend.
//!
//! The app is implemented in Rust/Yew. Browser wallet, WebCrypto, and extension
//! APIs are reached through `js_sys`/`wasm_bindgen`; MV3 glue code is kept in
//! strict TypeScript and compiled into ignored generated output.
#![deny(missing_docs)]

mod app;
mod browser_api;
mod connect;
mod controls;
mod custom;
mod dweb;
mod extension;
mod extension_bridge;
mod forms;
mod generation;
mod guide;
mod hex;
mod node;
mod onion;
mod peer_sync;
mod proof;
mod styles;
mod topology;
mod wallet;
mod workbench;

/// Mount the Yew app.
pub fn run() {
    if extension::is_offscreen_document() {
        yew::Renderer::<extension::HeadlessNode>::new().render();
    } else {
        yew::Renderer::<app::App>::new().render();
    }
}
