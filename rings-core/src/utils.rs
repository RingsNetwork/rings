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
    use wasm_bindgen::JsValue;

    use crate::err::Error;
    use crate::err::Result;

    /// Get property from a JsValue.
    pub fn get_property<T>(obj: &JsValue, key: String) -> Result<T>
        where T: From<JsValue>
    {
        let value = Reflect::get(&obj, &JsValue::from(key.clone()))
            .map_err(|_| Error::FailedOnGetProperty(key.clone()))?;
        Ok::<T, Error>(value.into())
    }

    /// Set Property to a JsValue.
    pub fn set_property(obj: &JsValue, key: String, value: impl Into<JsValue>) -> Result<bool> {
        Reflect::set(&obj, &JsValue::from(key.clone()), &value.into())
            .map_err(|_| Error::FailedOnSetProperty(key.clone()))
    }
}
