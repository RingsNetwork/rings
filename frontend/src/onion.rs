//! Onion proxy helpers for the browser frontend.

use std::fmt;

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
    /// Exact response body bytes returned by the onion exit.
    pub(crate) body: Vec<u8>,
}

/// Stable class for onion proxy failures that cross the JS promise boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OnionProxyFailureKind {
    /// The proxy failed before selecting a more specific route state.
    Generic,
    /// The route builder found no live exit for the requested HTTPS service.
    ExitUnavailable,
    /// The route builder found exits or relays, but no usable route.
    RouteUnavailable,
    /// The selected onion request did not complete before its deadline.
    RequestTimedOut,
}

/// Onion proxy error with a stable failure class and a diagnostic message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OnionProxyError {
    kind: OnionProxyFailureKind,
    message: String,
}

impl OnionProxyError {
    /// Build a generic onion proxy error.
    pub(crate) fn generic(message: impl Into<String>) -> Self {
        Self {
            kind: OnionProxyFailureKind::Generic,
            message: message.into(),
        }
    }

    /// Build an onion proxy error whose kind is derived once at the JS boundary.
    pub(crate) fn classified(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            kind: classify_onion_proxy_failure(&message),
            message,
        }
    }

    /// Stable class for browser-facing failure handling.
    pub(crate) fn kind(&self) -> OnionProxyFailureKind {
        self.kind
    }

    /// Diagnostic message for logs and UI text.
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for OnionProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OnionProxyError {}

impl From<String> for OnionProxyError {
    fn from(message: String) -> Self {
        Self::generic(message)
    }
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
        js_set(&object, "body", &body_js(&self.body).into())?;
        Ok(object.into())
    }

    /// Return a lossy UTF-8 representation for text-only UI output.
    pub(crate) fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
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
    provider: &Provider,
    request: OnionProxyRouteRequest,
) -> Result<OnionProxyRoute, OnionProxyError> {
    let target_authority = target_authority(&request.url)?;
    let proxy = provider
        .onion_https_proxy(request.options.hop_count, request.options.allow_short_paths)
        .map_err(|error| {
            OnionProxyError::generic(format!(
                "create onion proxy failed: {}",
                js_error_label(error.into())
            ))
        })?;
    let value = JsFuture::from(proxy.route(target_authority))
        .await
        .map_err(|error| {
            OnionProxyError::classified(format!(
                "build onion route failed: {}",
                js_error_label(error)
            ))
        })?;
    OnionProxyRoute::from_js(&value).map_err(OnionProxyError::generic)
}

/// Send one HTTPS request through an onion proxy.
pub(crate) async fn request(
    provider: &Provider,
    request: OnionProxyHttpRequest,
) -> Result<OnionProxyResponse, OnionProxyError> {
    target_authority(&request.url)?;
    let proxy = provider
        .onion_https_proxy(request.options.hop_count, request.options.allow_short_paths)
        .map_err(|error| {
            OnionProxyError::generic(format!(
                "create onion proxy failed: {}",
                js_error_label(error.into())
            ))
        })?;
    let value = JsFuture::from(proxy.request(request.url.clone(), request.client_request_js()?))
        .await
        .map_err(|error| {
            OnionProxyError::classified(format!(
                "onion proxy request failed: {}",
                js_error_label(error)
            ))
        })?;
    OnionProxyResponse::from_js(&value).map_err(OnionProxyError::generic)
}

fn classify_onion_proxy_failure(message: &str) -> OnionProxyFailureKind {
    if message.contains("no live onion exit offers service \"https\"") {
        OnionProxyFailureKind::ExitUnavailable
    } else if [
        "no live onion exit",
        "not enough relay candidates",
        "no onion route has a permitted first hop",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        OnionProxyFailureKind::RouteUnavailable
    } else if message.contains("onion HTTPS proxy request timed out")
        || message.contains("browser HTTPS proxy request timed out")
    {
        OnionProxyFailureKind::RequestTimedOut
    } else {
        OnionProxyFailureKind::Generic
    }
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

fn parse_body_js(value: JsValue) -> Result<Vec<u8>, String> {
    if let Some(body) = value.as_string() {
        return Ok(body.into_bytes());
    }
    if let Ok(bytes) = value.clone().dyn_into::<Uint8Array>() {
        let mut body = vec![0; bytes.length() as usize];
        bytes.copy_to(body.as_mut_slice());
        return Ok(body);
    }
    let array = Array::from(&value);
    let mut body = Vec::new();
    for index in 0..array.length() {
        let Some(byte) = array.get(index).as_f64() else {
            return Err("response body must contain bytes".to_string());
        };
        if !byte.is_finite() || !(0.0..=255.0).contains(&byte) || byte.fract() != 0.0 {
            return Err("response body contains an invalid byte".to_string());
        }
        body.push(byte as u8);
    }
    Ok(body)
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

        let parsed = route
            .to_js()
            .and_then(|value| super::OnionProxyRoute::from_js(&value));

        assert_eq!(parsed, Ok(route));
    }

    #[wasm_bindgen_test]
    fn route_from_js_accepts_legacy_exit_string() {
        let route = Object::new();
        let set_route = super::js_set(&route, "service", &JsValue::from_str("https"))
            .and_then(|()| super::js_set(&route, "exit", &JsValue::from_str("did:ring:exit")))
            .and_then(|()| {
                super::js_set(
                    &route,
                    "hops",
                    &super::string_array_js(&["did:ring:exit".to_string()]).into(),
                )
            });
        assert_eq!(set_route, Ok(()));

        let parsed = super::OnionProxyRoute::from_js(&route.into()).map(|route| route.exit);

        assert_eq!(parsed, Ok("did:ring:exit".to_string()));
    }

    #[wasm_bindgen_test]
    fn response_roundtrip_keeps_extension_body_parseable() {
        let response = super::OnionProxyResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: b"hello through onion".to_vec(),
        };

        let parsed = response
            .to_js()
            .and_then(|value| super::OnionProxyResponse::from_js(&value));

        assert_eq!(parsed, Ok(response));
    }

    #[wasm_bindgen_test]
    fn response_roundtrip_preserves_binary_body_bytes() {
        let response = super::OnionProxyResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "image/png".to_string())],
            body: vec![0xff, 0x00, 0x80],
        };

        let parsed = response
            .to_js()
            .and_then(|value| super::OnionProxyResponse::from_js(&value));

        assert_eq!(parsed, Ok(response));
    }

    #[wasm_bindgen_test]
    fn response_from_js_accepts_legacy_body_string() {
        let response = Object::new();
        let set_response = super::js_set(&response, "status", &JsValue::from_f64(200.0))
            .and_then(|()| super::js_set(&response, "headers", &Array::new().into()))
            .and_then(|()| {
                super::js_set(&response, "body", &JsValue::from_str("hello through onion"))
            });
        assert_eq!(set_response, Ok(()));

        let parsed = super::OnionProxyResponse::from_js(&response.into())
            .map(|response| response.body_text());

        assert_eq!(parsed, Ok("hello through onion".to_string()));
    }
}
