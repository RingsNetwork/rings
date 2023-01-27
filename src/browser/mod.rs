#![allow(clippy::unused_unit)]
#![allow(non_snake_case, non_upper_case_globals)]
/// rings-node browser support.
pub mod client;
pub mod jsonrpc_client;
pub mod utils;
use std::str::FromStr;

pub use self::client::*;
pub use self::jsonrpc_client::JsonRpcClient;
use crate::logging::browser::init_logging;
use crate::logging::browser::set_panic_hook;
use crate::prelude::wasm_bindgen;
use crate::prelude::wasm_bindgen::prelude::*;

/// set debug for wasm.
/// if `true` will print `Debug` message in console,
/// otherwise only print `error` message
#[wasm_bindgen]
pub fn debug(value: bool) {
    set_panic_hook();
    if value {
        init_logging(tracing::Level::DEBUG);
    } else {
        init_logging(tracing::Level::ERROR);
    }
}

/// set log_level
#[wasm_bindgen]
pub fn log_level(level: &str) {
    init_logging(tracing::Level::from_str(level).unwrap());
}
