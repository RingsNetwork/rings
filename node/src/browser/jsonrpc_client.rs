/// Simple json rpc client.
use std::sync::Arc;

use super::utils;
use crate::prelude::rings_core::utils::js_value;
use crate::prelude::wasm_bindgen;
use crate::prelude::wasm_bindgen::prelude::*;
use crate::prelude::*;

#[wasm_bindgen]
pub struct JsonRpcClient {
    client: Arc<jsonrpc_client::SimpleClient>,
}

#[wasm_bindgen]
impl JsonRpcClient {
    /// Create a new `JsonRpcClient`
    #[wasm_bindgen(constructor)]
    pub fn new(node: String) -> JsonRpcClient {
        let client = Arc::new(jsonrpc_client::SimpleClient::new_with_url(node.as_str()));
        JsonRpcClient { client }
    }

    pub fn request(&self, method: String, params: wasm_bindgen::JsValue) -> js_sys::Promise {
        let client = self.client.clone();
        future_to_promise(async move {
            log::debug!("request");
            let params =
                utils::parse_params(params).map_err(|e| JsError::new(e.to_string().as_str()))?;
            let resp = client
                .call_method(method.as_str(), params)
                .await
                .map_err(JsError::from)?;
            let result = js_value::serialize(&resp).map_err(JsError::from)?;
            Ok(result)
        })
    }
}
