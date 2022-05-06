#![feature(box_syntax)]

#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(not(feature = "wasm"))]
pub mod default;
