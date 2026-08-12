//! Browser-facing webview helpers.

use serde::Serialize;
use url::Url;

use crate::error::Result;
use crate::url::GatewayPrefix;

/// JavaScript bootstrap template marker used by tests and consumers.
pub const BOOTSTRAP_MARKER: &str = "__ringsWebviewGateway";

const BROWSER_RUNTIME: &str = include_str!("browser_runtime.mjs");
const BROWSER_TRANSFORMS: &str = include_str!("browser_runtime_transforms.mjs");

/// Typed configuration consumed by the browser runtime asset.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserBootstrapConfig<'a> {
    prefix: &'a str,
    target_base: &'a str,
    marker: &'a str,
    block_workers: bool,
    delegate_navigation: bool,
}

#[derive(Clone, Copy)]
enum BrowserHostPolicy {
    Web,
    Extension,
}

/// Serialized shape accepted by the browser onion HTTPS client.
#[cfg(any(target_family = "wasm", test))]
#[derive(Serialize)]
struct OnionHttpsRequest<'a> {
    method: &'a str,
    path: String,
    headers: Vec<(&'a str, &'a str)>,
    body: &'a [u8],
    max_response_body_bytes: usize,
}

#[cfg(any(target_family = "wasm", test))]
impl<'a> OnionHttpsRequest<'a> {
    fn new(
        request: &'a crate::types::GatewayRequest,
        body_limit: crate::transport::GatewayResponseBodyLimit,
    ) -> Self {
        Self {
            method: request.method.as_str(),
            path: request.path_and_query(),
            headers: request
                .headers
                .iter()
                .map(|header| (header.name.as_str(), header.value.as_str()))
                .collect(),
            body: request.body.as_slice(),
            max_response_body_bytes: body_limit.bytes(),
        }
    }
}

/// Build a runtime that rewrites browser-created URLs through the gateway prefix.
///
/// DOM URL rewriting remains useful in opaque-origin sandboxed documents. Dynamic `fetch` and
/// `XMLHttpRequest` routing additionally requires a trusted host bridge; an opaque document is
/// not controlled by the Service Worker that served its gateway URL. See the crate-level
/// opaque-origin deployment boundary.
pub fn bootstrap_script(gateway_prefix: &str, document_url: &Url) -> String {
    bootstrap_script_with_policy(gateway_prefix, document_url, BrowserHostPolicy::Web)
}

/// Build a browser runtime that delegates navigation and Worker effects to an extension host.
///
/// The caller must install fail-closed navigation, `Worker`, and `SharedWorker` adapters before
/// executing the returned script. All other unsupported browser-global network protocols remain
/// blocked.
pub fn bootstrap_script_with_extension_bridge(gateway_prefix: &str, document_url: &Url) -> String {
    bootstrap_script_with_policy(gateway_prefix, document_url, BrowserHostPolicy::Extension)
}

fn bootstrap_script_with_policy(
    gateway_prefix: &str,
    document_url: &Url,
    host_policy: BrowserHostPolicy,
) -> String {
    let config = BrowserBootstrapConfig {
        prefix: gateway_prefix,
        target_base: document_url.as_str(),
        marker: BOOTSTRAP_MARKER,
        block_workers: matches!(host_policy, BrowserHostPolicy::Web),
        delegate_navigation: matches!(host_policy, BrowserHostPolicy::Extension),
    };
    let config = match serde_json::to_string(&config) {
        Ok(config) => config,
        Err(_) => return String::new(),
    };

    let mut script = String::from("globalThis.__ringsWebviewBootstrapConfig=");
    script.push_str(&config);
    script.push_str(";\n");
    script.push_str(BROWSER_TRANSFORMS);
    script.push('\n');
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

#[cfg(target_family = "wasm")]
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
    use crate::transport::GatewayResponseBodyLimit;
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
        async fn send(
            &self,
            request: GatewayRequest,
            body_limit: GatewayResponseBodyLimit,
        ) -> Result<GatewayResponse> {
            let method = Reflect::get(&self.proxy, &JsValue::from_str("request"))
                .map_err(|error| WebviewError::Browser(format!("{error:?}")))?
                .dyn_into::<Function>()
                .map_err(|_| WebviewError::Browser("proxy.request is not callable".to_string()))?;
            let onion_request = OnionHttpsRequest::new(&request, body_limit);
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
            let response: GatewayResponse = serde_wasm_bindgen::from_value(response)
                .map_err(|error| WebviewError::Browser(error.to_string()))?;
            if response.body.len() > body_limit.bytes() {
                return Err(crate::error::TransportFailure::ResponseBodyTooLarge {
                    actual: response.body.len(),
                    limit: body_limit.bytes(),
                }
                .into());
            }
            Ok(response)
        }
    }

    pub use OnionProxyJsTransport as JsOnionProxyTransport;
}

#[cfg(target_family = "wasm")]
pub use wasm::JsOnionProxyTransport;

#[cfg(test)]
mod tests;
