//! rings-node browser support.
#![allow(clippy::unused_unit)]
#![allow(non_snake_case, non_upper_case_globals)]
pub mod client;
pub mod jsonrpc_client;
pub mod utils;

use std::str::FromStr;

pub use self::client::*;
pub use self::jsonrpc_client::JsonRpcClient;
use crate::prelude::wasm_bindgen;
use crate::prelude::wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsError> {
    utils::set_panic_hook();
    Ok(())
}

/// set debug for wasm.
/// if `true` will print `Debug` message in console,
/// otherwise only print `error` message
#[wasm_bindgen]
pub fn debug(value: bool) {
    if value {
        console_log::init_with_level(log::Level::Debug).ok();
    } else {
        console_log::init_with_level(log::Level::Error).ok();
    }
}

/// set log_level
#[wasm_bindgen]
pub fn log_level(level: &str) {
    console_log::init_with_level(log::Level::from_str(level).unwrap()).ok();
}
