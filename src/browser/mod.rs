/// rings-node browser support.
#![allow(clippy::unused_unit)]
#![allow(non_snake_case, non_upper_case_globals)]
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

/// set panic book for wasm.
#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsError> {
    set_panic_hook();
    Ok(())
}

/// set debug for wasm.
/// if `true` will print `Debug` message in console,
/// otherwise only print `error` message
#[wasm_bindgen]
pub fn debug(value: bool) {
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
