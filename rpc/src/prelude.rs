#[cfg(feature = "std")]
pub use reqwest;
#[cfg(feature = "wasm")]
pub use reqwest_wasm as reqwest;
