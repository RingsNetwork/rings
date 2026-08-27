use serde::de::DeserializeOwned;
use serde::Serialize;
use serde::Serializer;
use wasm_bindgen::JsValue;

use crate::error::Error;
use crate::error::Result;

/// From serde to JsValue
pub fn serialize(obj: &impl Serialize) -> Result<JsValue> {
    let serializer = serde_wasm_bindgen::Serializer::json_compatible();
    serializer
        .serialize_some(&obj)
        .map_err(Error::SerdeWasmBindgenError)
}

/// From JsValue to serde
pub fn deserialize<T: DeserializeOwned>(obj: impl Into<JsValue>) -> Result<T> {
    serde_wasm_bindgen::from_value(obj.into()).map_err(Error::SerdeWasmBindgenError)
}

/// From JsValue to serde_json::Value
pub fn json_value(obj: impl Into<JsValue>) -> Result<serde_json::Value> {
    let s = js_sys::JSON::stringify(&obj.into())
        .map_err(|_| Error::JsError("failed to stringify obj".to_string()))?;

    serde_json::from_str(&String::from(s)).map_err(Error::Deserialize)
}
