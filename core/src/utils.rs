//! Utils for ring-core
use chrono::Utc;

use crate::prelude::RTCIceConnectionState;

/// Get local utc timestamp (millisecond)
pub fn get_epoch_ms() -> u128 {
    Utc::now().timestamp_millis() as u128
}

/// convert RTCIceConnectionState to string
pub fn from_rtc_ice_connection_state(state: RTCIceConnectionState) -> String {
    match state {
        RTCIceConnectionState::New => "new",
        RTCIceConnectionState::Checking => "checking",
        RTCIceConnectionState::Connected => "connected",
        RTCIceConnectionState::Completed => "completed",
        RTCIceConnectionState::Failed => "failed",
        RTCIceConnectionState::Disconnected => "disconnected",
        RTCIceConnectionState::Closed => "closed",
        _ => "unknown",
    }
    .to_owned()
}

/// convert string to RTCIceConnectionState
#[allow(dead_code)]
pub fn into_rtc_ice_connection_state(value: &str) -> Option<RTCIceConnectionState> {
    Some(match value {
        "new" => RTCIceConnectionState::New,
        "checking" => RTCIceConnectionState::Checking,
        "connected" => RTCIceConnectionState::Connected,
        "completed" => RTCIceConnectionState::Completed,
        "failed" => RTCIceConnectionState::Failed,
        "disconnected" => RTCIceConnectionState::Disconnected,
        "closed" => RTCIceConnectionState::Closed,
        _ => return None,
    })
}

#[cfg(feature = "wasm")]
/// Toolset for wasm
pub mod js_value {
    use js_sys::Reflect;
    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use wasm_bindgen::JsValue;

    use crate::err::Error;
    use crate::err::Result;

    /// Get property from a JsValue.
    pub fn get<T: DeserializeOwned>(obj: &JsValue, key: impl Into<String>) -> Result<T> {
        let key = key.into();
        let value = Reflect::get(obj, &JsValue::from(key.clone()))
            .map_err(|_| Error::FailedOnGetProperty(key.clone()))?;
        serde_wasm_bindgen::from_value(value).map_err(Error::SerdeWasmBindgenError)
    }

    /// Set Property to a JsValue.
    pub fn set(obj: &JsValue, key: impl Into<String>, value: impl Into<JsValue>) -> Result<bool> {
        let key = key.into();
        Reflect::set(obj, &JsValue::from(key.clone()), &value.into())
            .map_err(|_| Error::FailedOnSetProperty(key.clone()))
    }

    /// From serde to JsValue
    pub fn serialize(obj: &impl Serialize) -> Result<JsValue> {
        serde_wasm_bindgen::to_value(&obj).map_err(Error::SerdeWasmBindgenError)
    }

    /// From JsValue to serde
    pub fn deserialize<T: DeserializeOwned>(obj: &(impl Into<JsValue> + Clone)) -> Result<T> {
        let value: JsValue = (*obj).clone().into();
        serde_wasm_bindgen::from_value(value).map_err(Error::SerdeWasmBindgenError)
    }
}

#[cfg(feature = "wasm")]
pub mod js_utils {
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::JsValue;

    pub fn window_sleep(millis: i32) -> wasm_bindgen_futures::JsFuture {
        let window = web_sys::window().unwrap();

        let promise = js_sys::Promise::new(&mut |resolve, _| {
            let func = Closure::once_into_js(move || {
                resolve.call0(&JsValue::NULL).unwrap();
            });
            window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    func.as_ref().unchecked_ref(),
                    millis,
                )
                .unwrap();
        });

        wasm_bindgen_futures::JsFuture::from(promise)
    }
}
