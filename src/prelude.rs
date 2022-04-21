#[cfg(feature = "client")]
pub use rings_core;
#[cfg(feature = "browser")]
pub use rings_core_wasm as rings_core;

#[cfg(feature = "client")]
pub use reqwest;
#[cfg(feature = "browser")]
pub use reqwest_wasm as reqwest;
