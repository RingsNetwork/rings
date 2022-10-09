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
