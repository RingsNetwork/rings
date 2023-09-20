//! Utils for ring-core
use chrono::Utc;

/// Get local utc timestamp (millisecond)
pub fn get_epoch_ms() -> u128 {
    Utc::now().timestamp_millis() as u128
}

#[cfg(feature = "wasm")]
/// Toolset for wasm
pub mod js_value {
    use js_sys::Reflect;
    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use wasm_bindgen::JsValue;

    use crate::error::Error;
    use crate::error::Result;

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

    pub enum Global {
        Window(web_sys::Window),
        WorkerGlobal(web_sys::WorkerGlobalScope),
        ServiceWorkerGlobal(web_sys::ServiceWorkerGlobalScope),
    }

    pub fn global() -> Option<Global> {
        let obj = JsValue::from(js_sys::global());
        if obj.has_type::<web_sys::Window>() {
            return Some(Global::Window(web_sys::Window::from(obj)));
        }
        if obj.has_type::<web_sys::WorkerGlobalScope>() {
            return Some(Global::WorkerGlobal(web_sys::WorkerGlobalScope::from(obj)));
        }
        if obj.has_type::<web_sys::ServiceWorkerGlobalScope>() {
            return Some(Global::ServiceWorkerGlobal(
                web_sys::ServiceWorkerGlobalScope::from(obj),
            ));
        }
        None
    }

    pub fn window_sleep(millis: i32) -> wasm_bindgen_futures::JsFuture {
        let promise = match global().unwrap() {
            Global::Window(window) => js_sys::Promise::new(&mut |resolve, _| {
                let func = Closure::once_into_js(move || {
                    resolve.call0(&JsValue::NULL).unwrap();
                });
                window
                    .set_timeout_with_callback_and_timeout_and_arguments_0(
                        func.as_ref().unchecked_ref(),
                        millis,
                    )
                    .unwrap();
            }),
            Global::WorkerGlobal(window) => js_sys::Promise::new(&mut |resolve, _| {
                let func = Closure::once_into_js(move || {
                    resolve.call0(&JsValue::NULL).unwrap();
                });
                window
                    .set_timeout_with_callback_and_timeout_and_arguments_0(
                        func.as_ref().unchecked_ref(),
                        millis,
                    )
                    .unwrap();
            }),
            Global::ServiceWorkerGlobal(window) => js_sys::Promise::new(&mut |resolve, _| {
                let func = Closure::once_into_js(move || {
                    resolve.call0(&JsValue::NULL).unwrap();
                });
                window
                    .set_timeout_with_callback_and_timeout_and_arguments_0(
                        func.as_ref().unchecked_ref(),
                        millis,
                    )
                    .unwrap();
            }),
        };
        wasm_bindgen_futures::JsFuture::from(promise)
    }
}
