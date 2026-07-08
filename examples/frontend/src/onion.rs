//! Onion proxy helpers for the browser frontend.

use std::sync::Arc;

use js_sys::Array;
use js_sys::Object;
use js_sys::Uint8Array;
use rings_node::provider::Provider;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::Url;

use crate::browser_api::js_bool_field;
use crate::browser_api::js_error_label;
use crate::browser_api::js_prop;
use crate::browser_api::js_set;
use crate::browser_api::js_string_field;

const DEFAULT_HOP_COUNT: usize = 3;

/// Onion HTTPS proxy route-selection options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OnionProxyOptions {
    /// Desired hop count including the exit. `0` delegates to node defaults.
    pub(crate) hop_count: usize,
    /// Allow fewer hops when the live network cannot satisfy the requested count.
    pub(crate) allow_short_paths: bool,
}

/// One route probe request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OnionProxyRouteRequest {
    /// Absolute HTTPS URL used to derive the target authority.
    pub(crate) url: String,
    /// Route-selection options.
    pub(crate) options: OnionProxyOptions,
}

/// One proxied HTTPS request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OnionProxyHttpRequest {
    /// Absolute HTTPS URL sent through the selected exit.
    pub(crate) url: String,
    /// HTTP method.
    pub(crate) method: String,
    /// HTTP headers.
    pub(crate) headers: Vec<(String, String)>,
    /// UTF-8 request body bytes.
    pub(crate) body: Vec<u8>,
    /// Route-selection options.
    pub(crate) options: OnionProxyOptions,
}

/// Browser-displayable route result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OnionProxyRoute {
    /// Onion service name selected by the core route builder.
    pub(crate) service: String,
    /// Ordered DID hops ending with the exit.
    pub(crate) hops: Vec<String>,
    /// Exit DID.
    pub(crate) exit: String,
}

/// Browser-displayable proxied response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OnionProxyResponse {
    /// HTTP status code.
    pub(crate) status: u16,
    /// Response headers.
    pub(crate) headers: Vec<(String, String)>,
    /// Lossy UTF-8 response body for the demo panel.
    pub(crate) body: String,
}

impl Default for OnionProxyOptions {
    fn default() -> Self {
        Self {
            hop_count: DEFAULT_HOP_COUNT,
            allow_short_paths: true,
        }
    }
}

impl OnionProxyOptions {
    /// Parse route options from UI state.
    pub(crate) fn from_input(hop_count: &str, allow_short_paths: bool) -> Result<Self, String> {
        let hop_count = match hop_count.trim() {
            "" => DEFAULT_HOP_COUNT,
            value => value
                .parse::<usize>()
                .map_err(|error| format!("invalid hop count: {error}"))?,
        };
        Ok(Self {
            hop_count,
            allow_short_paths,
        })
    }

    fn from_js(message: &JsValue) -> Result<Self, String> {
        Ok(Self {
            hop_count: optional_usize_field(message, "hopCount", DEFAULT_HOP_COUNT)?,
            allow_short_paths: js_bool_field(message, "allowShortPaths").unwrap_or(true),
        })
    }

    fn write_js(&self, object: &Object) -> Result<(), String> {
        js_set(
            object,
            "hopCount",
            &JsValue::from_f64(self.hop_count as f64),
        )?;
        js_set(
            object,
            "allowShortPaths",
            &JsValue::from_bool(self.allow_short_paths),
        )
    }
}

impl OnionProxyRouteRequest {
    /// Parse one route request from an extension runtime message.
    pub(crate) fn from_js(message: &JsValue) -> Result<Self, String> {
        Ok(Self {
            url: required_string_field(message, "url", "enter an HTTPS URL")?,
            options: OnionProxyOptions::from_js(message)?,
        })
    }

    /// Convert this request to the extension bridge payload shape.
    pub(crate) fn to_js(&self) -> Result<JsValue, String> {
        let object = Object::new();
        js_set(&object, "url", &JsValue::from_str(&self.url))?;
        self.options.write_js(&object)?;
        Ok(object.into())
    }
}

impl OnionProxyHttpRequest {
    /// Parse one proxied HTTPS request from an extension runtime message.
    pub(crate) fn from_js(message: &JsValue) -> Result<Self, String> {
        Ok(Self {
            url: required_string_field(message, "url", "enter an HTTPS URL")?,
            method: required_string_field(message, "method", "enter an HTTP method")?,
            headers: parse_headers_js(js_prop(message, "headers")?)?,
            body: js_string_field(message, "body")
                .unwrap_or_default()
                .into_bytes(),
            options: OnionProxyOptions::from_js(message)?,
        })
    }

    /// Convert this request to the extension bridge payload shape.
    pub(crate) fn to_js(&self) -> Result<JsValue, String> {
        let object = Object::new();
        js_set(&object, "url", &JsValue::from_str(&self.url))?;
        js_set(&object, "method", &JsValue::from_str(&self.method))?;
        js_set(&object, "headers", &headers_js(&self.headers).into())?;
        js_set(
            &object,
            "body",
            &JsValue::from_str(&String::from_utf8_lossy(&self.body)),
        )?;
        self.options.write_js(&object)?;
        Ok(object.into())
    }

    fn client_request_js(&self) -> Result<JsValue, String> {
        let object = Object::new();
        js_set(&object, "method", &JsValue::from_str(&self.method))?;
        js_set(&object, "headers", &headers_js(&self.headers).into())?;
        js_set(&object, "body", &body_js(&self.body).into())?;
        Ok(object.into())
    }
}

impl OnionProxyRoute {
    /// Parse a route DTO returned by the browser provider.
    pub(crate) fn from_js(value: &JsValue) -> Result<Self, String> {
        let exit = js_prop(value, "exit")?;
        Ok(Self {
            service: js_string_field(value, "service")?,
            hops: parse_string_array(js_prop(value, "hops")?)?,
            exit: parse_route_exit_js(&exit)?,
        })
    }

    /// Convert the route view to an extension response payload.
    pub(crate) fn to_js(&self) -> Result<JsValue, String> {
        let object = Object::new();
        let exit = Object::new();
        js_set(&exit, "did", &JsValue::from_str(&self.exit))?;
        js_set(&object, "service", &JsValue::from_str(&self.service))?;
        js_set(&object, "exit", &exit.into())?;
        js_set(&object, "hops", &string_array_js(&self.hops).into())?;
        Ok(object.into())
    }

    /// Compact text for the WorkBench output pane.
    pub(crate) fn summary(&self) -> String {
        format!(
            "service: {}\nexit: {}\nhops:\n{}",
            self.service,
            self.exit,
            self.hops
                .iter()
                .map(|hop| format!("- {hop}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

impl OnionProxyResponse {
    /// Parse a response returned by the browser provider.
    pub(crate) fn from_js(value: &JsValue) -> Result<Self, String> {
        Ok(Self {
            status: parse_status(js_prop(value, "status")?)?,
            headers: parse_headers_js(js_prop(value, "headers")?)?,
            body: parse_body_js(js_prop(value, "body")?)?,
        })
    }

    /// Convert the response view to an extension response payload.
    pub(crate) fn to_js(&self) -> Result<JsValue, String> {
        let object = Object::new();
        js_set(
            &object,
            "status",
            &JsValue::from_f64(f64::from(self.status)),
        )?;
        js_set(&object, "headers", &headers_js(&self.headers).into())?;
        js_set(&object, "body", &body_js(self.body.as_bytes()).into())?;
        Ok(object.into())
    }
}

/// Parse editable header lines from the WorkBench.
pub(crate) fn parse_header_lines(input: &str) -> Result<Vec<(String, String)>, String> {
    let mut headers = Vec::new();
    for (index, line) in input.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(format!("header line {} must use Name: value", index + 1));
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(format!("header line {} has an empty name", index + 1));
        }
        headers.push((name, value.trim().to_string()));
    }
    Ok(headers)
}

/// Format headers for the WorkBench output pane.
pub(crate) fn format_headers(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build a route through an HTTPS onion exit.
pub(crate) async fn route(
    provider: &Arc<Provider>,
    request: OnionProxyRouteRequest,
) -> Result<OnionProxyRoute, String> {
    let target_authority = target_authority(&request.url)?;
    let proxy = provider
        .onion_https_proxy(request.options.hop_count, request.options.allow_short_paths)
        .map_err(|error| format!("create onion proxy failed: {error:?}"))?;
    let value = JsFuture::from(proxy.route(target_authority))
        .await
        .map_err(|error| format!("build onion route failed: {}", js_error_label(error)))?;
    OnionProxyRoute::from_js(&value)
}

/// Send one HTTPS request through an onion proxy.
pub(crate) async fn request(
    provider: &Arc<Provider>,
    request: OnionProxyHttpRequest,
) -> Result<OnionProxyResponse, String> {
    target_authority(&request.url)?;
    let proxy = provider
        .onion_https_proxy(request.options.hop_count, request.options.allow_short_paths)
        .map_err(|error| format!("create onion proxy failed: {error:?}"))?;
    let value = JsFuture::from(proxy.request(request.url.clone(), request.client_request_js()?))
        .await
        .map_err(|error| format!("onion proxy request failed: {}", js_error_label(error)))?;
    OnionProxyResponse::from_js(&value)
}

fn target_authority(url: &str) -> Result<String, String> {
    let parsed = Url::new(url).map_err(js_error_label)?;
    if parsed.protocol() != "https:" {
        return Err("onion proxy only accepts https URLs".to_string());
    }
    if !parsed.username().is_empty() || !parsed.password().is_empty() {
        return Err("onion proxy URLs must not contain userinfo".to_string());
    }
    let authority = parsed.host();
    if authority.trim().is_empty() {
        Err("HTTPS URL must include a host".to_string())
    } else if parsed.port().is_empty() {
        Ok(format!("{authority}:443"))
    } else {
        Ok(authority)
    }
}

#[cfg(test)]
mod tests {
    use js_sys::Array;
    use js_sys::Object;
    use wasm_bindgen::JsValue;
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::target_authority;

    #[wasm_bindgen_test]
    fn target_authority_adds_default_https_port() {
        assert_eq!(
            target_authority("https://Example.COM/search?q=rust").as_deref(),
            Ok("example.com:443")
        );
    }

    #[wasm_bindgen_test]
    fn target_authority_preserves_explicit_port() {
        assert_eq!(
            target_authority("https://Example.COM:8443/original").as_deref(),
            Ok("example.com:8443")
        );
    }

    #[wasm_bindgen_test]
    fn route_roundtrip_keeps_extension_exit_parseable() {
        let route = super::OnionProxyRoute {
            service: "https".to_string(),
            hops: vec!["did:ring:relay".to_string(), "did:ring:exit".to_string()],
            exit: "did:ring:exit".to_string(),
        };

        let parsed = super::OnionProxyRoute::from_js(&route.to_js().unwrap()).unwrap();

        assert_eq!(parsed, route);
    }

    #[wasm_bindgen_test]
    fn route_from_js_accepts_legacy_exit_string() {
        let route = Object::new();
        super::js_set(&route, "service", &JsValue::from_str("https")).unwrap();
        super::js_set(&route, "exit", &JsValue::from_str("did:ring:exit")).unwrap();
        super::js_set(
            &route,
            "hops",
            &super::string_array_js(&["did:ring:exit".to_string()]).into(),
        )
        .unwrap();

        let parsed = super::OnionProxyRoute::from_js(&route.into()).unwrap();

        assert_eq!(parsed.exit, "did:ring:exit");
    }

    #[wasm_bindgen_test]
    fn response_roundtrip_keeps_extension_body_parseable() {
        let response = super::OnionProxyResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: "hello through onion".to_string(),
        };

        let parsed = super::OnionProxyResponse::from_js(&response.to_js().unwrap()).unwrap();

        assert_eq!(parsed, response);
    }

    #[wasm_bindgen_test]
    fn response_from_js_accepts_legacy_body_string() {
        let response = Object::new();
        super::js_set(&response, "status", &JsValue::from_f64(200.0)).unwrap();
        super::js_set(&response, "headers", &Array::new().into()).unwrap();
        super::js_set(&response, "body", &JsValue::from_str("hello through onion")).unwrap();

        let parsed = super::OnionProxyResponse::from_js(&response.into()).unwrap();

        assert_eq!(parsed.body, "hello through onion");
    }
}

fn optional_usize_field(
    object: &JsValue,
    field: &'static str,
    default: usize,
) -> Result<usize, String> {
    let value = js_prop(object, field)?;
    if value.is_null() || value.is_undefined() {
        return Ok(default);
    }
    let Some(number) = value.as_f64() else {
        return Err(format!("{field} must be a number"));
    };
    if !number.is_finite() || number < 0.0 || number.fract() != 0.0 {
        return Err(format!("{field} must be a non-negative integer"));
    }
    if number > u32::MAX as f64 {
        return Err(format!("{field} is too large"));
    }
    Ok(number as usize)
}

fn required_string_field(
    object: &JsValue,
    field: &'static str,
    empty_message: &'static str,
) -> Result<String, String> {
    let value = js_string_field(object, field)?.trim().to_string();
    if value.is_empty() {
        Err(empty_message.to_string())
    } else {
        Ok(value)
    }
}

fn parse_status(value: JsValue) -> Result<u16, String> {
    let Some(status) = value.as_f64() else {
        return Err("response status must be a number".to_string());
    };
    if !status.is_finite() || status < 0.0 || status.fract() != 0.0 {
        return Err("response status must be a non-negative integer".to_string());
    }
    if status > f64::from(u16::MAX) {
        return Err("response status exceeds u16".to_string());
    }
    Ok(status as u16)
}

fn parse_headers_js(value: JsValue) -> Result<Vec<(String, String)>, String> {
    if value.is_null() || value.is_undefined() {
        return Ok(Vec::new());
    }
    let array = Array::from(&value);
    let mut headers = Vec::new();
    for index in 0..array.length() {
        let pair = Array::from(&array.get(index));
        if pair.length() < 2 {
            return Err("header pairs must contain name and value".to_string());
        }
        let name = pair
            .get(0)
            .as_string()
            .ok_or_else(|| "header name must be a string".to_string())?;
        let value = pair
            .get(1)
            .as_string()
            .ok_or_else(|| "header value must be a string".to_string())?;
        headers.push((name, value));
    }
    Ok(headers)
}

fn parse_string_array(value: JsValue) -> Result<Vec<String>, String> {
    let array = Array::from(&value);
    let mut out = Vec::new();
    for index in 0..array.length() {
        let value = array
            .get(index)
            .as_string()
            .ok_or_else(|| "expected a string array".to_string())?;
        out.push(value);
    }
    Ok(out)
}

fn parse_route_exit_js(value: &JsValue) -> Result<String, String> {
    if let Some(exit) = value.as_string() {
        return Ok(exit);
    }
    js_string_field(value, "did")
}

fn parse_body_js(value: JsValue) -> Result<String, String> {
    if let Some(body) = value.as_string() {
        return Ok(body);
    }
    if let Ok(bytes) = value.clone().dyn_into::<Uint8Array>() {
        let mut body = vec![0; bytes.length() as usize];
        bytes.copy_to(body.as_mut_slice());
        return Ok(String::from_utf8_lossy(&body).into_owned());
    }
    let array = Array::from(&value);
    let mut body = Vec::new();
    for index in 0..array.length() {
        let Some(byte) = array.get(index).as_f64() else {
            return Err("response body must contain bytes".to_string());
        };
        if !byte.is_finite() || byte < 0.0 || byte > 255.0 || byte.fract() != 0.0 {
            return Err("response body contains an invalid byte".to_string());
        }
        body.push(byte as u8);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn headers_js(headers: &[(String, String)]) -> Array {
    let array = Array::new();
    for (name, value) in headers {
        let pair = Array::new();
        pair.push(&JsValue::from_str(name));
        pair.push(&JsValue::from_str(value));
        array.push(&pair);
    }
    array
}

fn body_js(body: &[u8]) -> Array {
    let array = Array::new();
    for byte in body {
        array.push(&JsValue::from_f64(f64::from(*byte)));
    }
    array
}

fn string_array_js(values: &[String]) -> Array {
    let array = Array::new();
    for value in values {
        array.push(&JsValue::from_str(value));
    }
    array
}
