//! Rings browser frontend.
//!
//! The app is implemented in Rust/Yew. Browser wallet and WebCrypto APIs are
//! reached through `js_sys`/`wasm_bindgen`; there is no JavaScript or TypeScript
//! application source in this example.

mod app;
mod browser_api;
mod connect;
mod controls;
mod custom;
mod dweb;
mod extension;
mod forms;
mod generation;
mod hex;
mod node;
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
