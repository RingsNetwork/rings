use super::Error;

#[cfg(all(feature = "wasm", target_family = "wasm"))]
impl From<Error> for wasm_bindgen::JsValue {
    fn from(err: Error) -> Self {
        wasm_bindgen::JsValue::from_str(&err.to_string())
    }
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
impl From<js_sys::Error> for Error {
    fn from(err: js_sys::Error) -> Self {
        Error::JsError(err.to_string().into())
    }
}
