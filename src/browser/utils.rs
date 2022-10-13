/// A parser convert JsValue to jsonrpc_core::Params.
/// # Examples
///
/// ```
/// let nv = JsValue::null();
/// match parse_params(nv) {
///     Ok(v) => {
///         assert!(v == jsonrpc_core::Params::None, "not null");
///     }
///     Err(_) => {
///         panic!("Exception");
///     }
/// }
///
/// let arr_v = js_sys::Array::new();
/// arr_v.push(&JsValue::from_str("test1"));
/// let jv: &JsValue = arr_v.as_ref();
/// let value2 = browser::utils::parse_params(jv.clone()).unwrap();
/// if let jsonrpc_core::Params::Array(v) = value2 {
///     assert!(v.len() == 1, "value2.len got {}, expect 1", v.len());
///     let v0 = v.get(0).unwrap();
///     assert!(v0.is_string(), "v0 not string");
///     assert!(v0.as_str() == Some("test1"), "v0 value {:?}", v0.as_str());
/// } else {
///     panic!("value2 not array");
/// }
/// ```
use crate::error::Error;
use crate::prelude::js_sys;
use crate::prelude::wasm_bindgen::prelude::*;

pub fn parse_params(params: JsValue) -> Result<jsonrpc_core::Params, Error> {
    let params = if params.is_null() {
        jsonrpc_core::Params::None
    } else if js_sys::Array::is_array(&params) {
        let arr = js_sys::Array::from(&params);
        let v = arr
            .iter()
            .flat_map(|x| x.into_serde::<serde_json::Value>().ok())
            .collect::<Vec<serde_json::Value>>();
        jsonrpc_core::Params::Array(v)
    } else if params.is_object() {
        let mut s_map = serde_json::Map::new();
        let obj = js_sys::Object::from(params);
        let entries = js_sys::Object::entries(&obj);
        for e in entries.iter() {
            if js_sys::Array::is_array(&e) {
                let arr = js_sys::Array::from(&e);
                if arr.length() != 2 {
                    continue;
                }
                let k = arr.get(0);
                let v = arr.get(1);
                s_map.insert(k.as_string().unwrap(), v.into_serde().unwrap());
            }
        }
        jsonrpc_core::Params::Map(s_map)
    } else {
        return Err(Error::JsError("unsupport params".to_owned()));
    };
    Ok(params)
}
