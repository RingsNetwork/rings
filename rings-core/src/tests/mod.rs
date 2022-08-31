#[cfg(feature = "wasm")]
mod wasm;

#[cfg(all(not(feature = "wasm")))]
mod default;
