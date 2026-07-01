//! Unified Rings browser node demo.
//!
//! The app is implemented in Rust/Yew. Browser wallet and WebCrypto APIs are
//! reached through `js_sys`/`wasm_bindgen`; there is no JavaScript or TypeScript
//! application source in this example.

mod app;
mod custom;
mod dweb;
mod node;
mod proof;
mod styles;
mod wallet;

/// Mount the Yew app.
pub fn run() {
    yew::Renderer::<app::App>::new().render();
}
