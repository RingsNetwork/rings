#[cfg(test)]
pub mod test {
    use rings_core::types::ice_transport::ice_server::IceServer;
    use js_sys::Array;
    use js_sys::Reflect;
    use std::str::FromStr;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::JsValue;
    use wasm_bindgen_test::wasm_bindgen_test;
    use web_sys::RtcIceServer;

    #[wasm_bindgen_test]
    fn test_transform() {
        let c = "turn://ryan@ethereum.org:9090/nginx/v2";
        let ret_a: RtcIceServer = IceServer::from_str(c).unwrap().into();
        let ret_urls = Reflect::get(&ret_a, &JsValue::from_str("urls")).unwrap();
        let urls: &Array = ret_urls.as_ref().unchecked_ref();
        assert_eq!(
            urls.get(0),
            JsValue::from_str("turn:ethereum.org:9090/nginx/v2")
        );
        assert_eq!(urls.get(1), JsValue::UNDEFINED);

        let username = Reflect::get(&ret_a, &JsValue::from_str("username")).unwrap();
        assert_eq!(username, JsValue::from_str("ryan"));
    }
}
