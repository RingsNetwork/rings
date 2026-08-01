//! Browser-facing webview helpers.

use serde::Serialize;
use url::Url;

use crate::error::Result;
use crate::url::GatewayPrefix;

/// JavaScript bootstrap template marker used by tests and consumers.
pub const BOOTSTRAP_MARKER: &str = "__ringsWebviewGateway";

const BROWSER_RUNTIME: &str = include_str!("browser_runtime.mjs");

/// Typed configuration consumed by the browser runtime asset.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserBootstrapConfig<'a> {
    prefix: &'a str,
    target_base: &'a str,
    marker: &'a str,
}

/// Serialized shape accepted by the browser onion HTTPS client.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Serialize)]
struct OnionHttpsRequest<'a> {
    method: &'a str,
    path: String,
    headers: Vec<(&'a str, &'a str)>,
    body: &'a [u8],
}

#[cfg(any(target_arch = "wasm32", test))]
impl<'a> From<&'a crate::types::GatewayRequest> for OnionHttpsRequest<'a> {
    fn from(request: &'a crate::types::GatewayRequest) -> Self {
        Self {
            method: request.method.as_str(),
            path: request.path_and_query(),
            headers: request
                .headers
                .iter()
                .map(|header| (header.name.as_str(), header.value.as_str()))
                .collect(),
            body: request.body.as_slice(),
        }
    }
}

/// Build a small runtime that routes browser-created URLs through the gateway prefix.
pub fn bootstrap_script(gateway_prefix: &str, document_url: &Url) -> String {
    let config = BrowserBootstrapConfig {
        prefix: gateway_prefix,
        target_base: document_url.as_str(),
        marker: BOOTSTRAP_MARKER,
    };
    let config = match serde_json::to_string(&config) {
        Ok(config) => config,
        Err(_) => return String::new(),
    };

    let mut script = String::from("globalThis.__ringsWebviewBootstrapConfig=");
    script.push_str(&config);
    script.push_str(";\n");
    script.push_str(BROWSER_RUNTIME);
    script
}

/// Resolve a runtime URL the same way the bootstrap does and encode it for the gateway.
pub fn runtime_gateway_url(
    gateway_prefix: &GatewayPrefix,
    document_url: &Url,
    input: &str,
) -> Result<Option<String>> {
    gateway_prefix.rewrite_url_value(document_url, input)
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use async_trait::async_trait;
    use js_sys::Function;
    use js_sys::Promise;
    use js_sys::Reflect;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::JsValue;
    use wasm_bindgen_futures::JsFuture;

    use super::OnionHttpsRequest;
    use crate::error::Result;
    use crate::error::WebviewError;
    use crate::transport::GatewayTransport;
    use crate::types::GatewayRequest;
    use crate::types::GatewayResponse;

    /// Browser adapter for JS objects that expose `request(url, request)`.
    pub struct OnionProxyJsTransport {
        proxy: JsValue,
    }

    impl OnionProxyJsTransport {
        /// Build a transport around an existing browser onion proxy object.
        pub fn new(proxy: JsValue) -> Self {
            Self { proxy }
        }
    }

    #[async_trait(?Send)]
    impl GatewayTransport for OnionProxyJsTransport {
        async fn send(&self, request: GatewayRequest) -> Result<GatewayResponse> {
            let method = Reflect::get(&self.proxy, &JsValue::from_str("request"))
                .map_err(|error| WebviewError::Browser(format!("{error:?}")))?
                .dyn_into::<Function>()
                .map_err(|_| WebviewError::Browser("proxy.request is not callable".to_string()))?;
            let onion_request = OnionHttpsRequest::from(&request);
            let request_value = serde_wasm_bindgen::to_value(&onion_request)
                .map_err(|error| WebviewError::Browser(error.to_string()))?;
            let value = method
                .call2(
                    &self.proxy,
                    &JsValue::from_str(request.target.as_str()),
                    &request_value,
                )
                .map_err(|error| WebviewError::Browser(format!("{error:?}")))?;
            let response = JsFuture::from(Promise::from(value))
                .await
                .map_err(|error| WebviewError::Browser(format!("{error:?}")))?;
            serde_wasm_bindgen::from_value(response)
                .map_err(|error| WebviewError::Browser(error.to_string()))
        }
    }

    pub use OnionProxyJsTransport as JsOnionProxyTransport;
}

#[cfg(target_arch = "wasm32")]
pub use wasm::JsOnionProxyTransport;

#[cfg(test)]
mod tests;
