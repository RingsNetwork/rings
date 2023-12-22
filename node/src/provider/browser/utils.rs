#![warn(missing_docs)]
//! Utils for browser client
use js_sys;
use wasm_bindgen::prelude::*;

use crate::error::Error;

/// Convert JsValue to serde_json::Value.
pub fn parse_params(params: JsValue) -> Result<serde_json::Value, Error> {
    let s = js_sys::JSON::stringify(&params)
        .map_err(|_| Error::JsError("failed to stringify params".to_string()))?;

    serde_json::from_str(&String::from(s))
        .map_err(|_| Error::JsError("failed to parse params".to_string()))
}
