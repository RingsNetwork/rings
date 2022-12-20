//! Utils for ring-core
use chrono::Utc;

/// Get local utc timestamp (millisecond)
pub fn get_epoch_ms() -> u128 {
    Utc::now().timestamp_millis() as u128
}

#[cfg(feature = "wasm")]
/// Toolset for wasm
pub mod wasm {
    use js_sys::Reflect;
    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use wasm_bindgen::JsValue;

    use crate::err::Error;
    use crate::err::Result;

    /// Get property from a JsValue.
    pub fn js_get<T: DeserializeOwned>(obj: &JsValue, key: impl Into<String>) -> Result<T> {
        let key = key.into();
        let value = Reflect::get(obj, &JsValue::from(key.clone()))
            .map_err(|_| Error::FailedOnGetProperty(key.clone()))?;
        serde_wasm_bindgen::from_value(value).map_err(Error::SerdeWasmBindgenError)
    }

    /// Set Property to a JsValue.
    pub fn js_set(
        obj: &JsValue,
        key: impl Into<String>,
        value: impl Into<JsValue>,
    ) -> Result<bool> {
        let key = key.into();
        Reflect::set(obj, &JsValue::from(key.clone()), &value.into())
            .map_err(|_| Error::FailedOnSetProperty(key.clone()))
    }

    /// From serde to JsValue
    pub fn serialize(obj: impl Serialize) -> Result<JsValue> {
        serde_wasm_bindgen::to_value(&obj).map_err(Error::SerdeWasmBindgenError)
    }

    /// From JsValue to serde
    pub fn deserialize<T: DeserializeOwned>(obj: impl Into<JsValue>) -> Result<T> {
        serde_wasm_bindgen::from_value(obj.into()).map_err(Error::SerdeWasmBindgenError)
    }
}
