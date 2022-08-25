#![feature(box_syntax)]

#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(all(not(feature = "wasm"), not(feature = "dummy")))]
pub mod default;
