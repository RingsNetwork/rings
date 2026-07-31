//! Frontend host adapter for the reusable Rings webview gateway.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use futures::channel::oneshot;
use js_sys::Array;
use js_sys::Object;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_node::provider::Provider;
use rings_webview::browser::bootstrap_script;
use rings_webview::ConcurrentWebviewGateway;
use rings_webview::GatewayCredentials;
use rings_webview::GatewayFailure;
use rings_webview::GatewayHeader;
use rings_webview::GatewayPrefix;
use rings_webview::GatewayRequest;
use rings_webview::GatewayRequestKind;
use rings_webview::GatewayResponse;
use rings_webview::GatewayRoute;
use rings_webview::GatewayRoutePolicy;
use rings_webview::GatewayRouteRejection;
use rings_webview::GatewayTransport;
use rings_webview::Result as WebviewResult;
use rings_webview::TargetUrl;
use rings_webview::WebviewError;
use url::Url;
use wasm_bindgen::JsValue;

use crate::onion;

mod browser_gateway;
#[cfg(test)]
use self::browser_gateway::browser_request_id;
pub(crate) use self::browser_gateway::clear_browser_gateway;
pub(crate) use self::browser_gateway::install_browser_gateway;
pub(crate) use self::browser_gateway::open_webview_popup;
pub(crate) use self::browser_gateway::register_browser_gateway;

pub(crate) const GATEWAY_PREFIX: &str = "/webview/";
const MAX_CONCURRENT_GATEWAY_REQUESTS: usize = 6;
const WEBVIEW_OVERLAY_LOADER: &str = r#"
(() => {
  "use strict";

  const marker = "__ringsWebviewDebugOverlay";
  if (globalThis[marker]?.installed) {
    globalThis[marker].mount?.();
    return;
  }
  if (document.querySelector("script[data-rings-webview-overlay-loader]")) return;
  if (globalThis.__ringsWebviewGateway?.loadLocalScript?.("/assets/webview-overlay.js", "data-rings-webview-overlay-loader")) return;

  const script = document.createElement("script");
  script.src = "/assets/webview-overlay.js";
  script.async = false;
  script.dataset.ringsWebviewOverlayLoader = "";
  (document.head || document.documentElement).append(script);
})();
"#;

/// Runtime onion routing settings shared by the local node and WebView gateway.
#[derive(Clone)]
pub struct WebviewOnionSettings {
    allow_short_paths: Rc<Cell<bool>>,
}

impl WebviewOnionSettings {
    /// Build settings with the current short-path opt-in state.
    pub fn new(allow_short_paths: bool) -> Self {
        Self {
            allow_short_paths: Rc::new(Cell::new(allow_short_paths)),
        }
    }

    /// Update whether the WebView may use fewer onion hops than requested.
    pub(crate) fn set_allow_short_paths(&self, allow_short_paths: bool) {
        self.allow_short_paths.set(allow_short_paths);
    }

    fn options(&self) -> onion::OnionProxyOptions {
        onion::OnionProxyOptions {
            allow_short_paths: self.allow_short_paths.get(),
            ..onion::OnionProxyOptions::default()
        }
    }
}

impl Default for WebviewOnionSettings {
    fn default() -> Self {
        Self::new(false)
    }
}

async fn dispatch_browser_request(gateway: WebviewNode, request: JsValue) -> JsValue {
    let request = match browser_host_request(&request) {
        Ok(request) => request,
        Err(error) => return browser_failure(400, error),
    };
    match gateway.handle(request).await {
        Ok(WebviewHostOutcome::Response(response)) => browser_response(response),
        Ok(WebviewHostOutcome::Redirect(location)) => browser_redirect(location.as_str()),
        Ok(WebviewHostOutcome::AllowControlled) => browser_failure(
            404,
            "controlled asset is outside the gateway route".to_string(),
        ),
        Ok(WebviewHostOutcome::Reject(reason)) => {
            browser_failure(403, format!("rejected: {reason:?}"))
        }
        Err(WebviewError::Cors(error)) => browser_failure(403, format!("CORS rejected: {error}")),
        Err(error) => browser_transport_failure(error),
    }
}

fn browser_host_request(value: &JsValue) -> Result<WebviewHostRequest, String> {
    let requested = parse_target(
        crate::browser_api::js_string_field(value, "requested")?,
        "requested",
    )?;
    let source_target = optional_target(value, "sourceTarget")?;
    let method = crate::browser_api::js_string_field(value, "method")?;
    let headers = browser_headers(value)?;
    let body = browser_body(value)?;
    let credentials = browser_credentials(value)?;
    let kind = crate::browser_api::js_string_field(value, "kind")?;
    let top_level_navigation = crate::browser_api::js_bool_field(value, "topLevelNavigation")
        .unwrap_or(kind == "navigation");
    let request = match kind.as_str() {
        "navigation" => Ok(WebviewHostRequest::navigation_with_payload(
            requested, method, headers, body,
        )
        .with_source_target(source_target)
        .with_top_level_navigation(top_level_navigation)),
        "subresource" => Ok(WebviewHostRequest::subresource(
            requested,
            source_target,
            method,
            headers,
            body,
        )),
        "fetch" => Ok(WebviewHostRequest::fetch(
            requested,
            source_target.ok_or_else(|| "fetch has no controlled source".to_string())?,
            method,
            headers,
            body,
        )),
        "xhr" => Ok(WebviewHostRequest::xhr(
            requested,
            source_target.ok_or_else(|| "xhr has no controlled source".to_string())?,
            method,
            headers,
            body,
        )),
        kind => Err(format!("unknown gateway request kind {kind}")),
    }?;
    Ok(request.with_credentials(credentials))
}

fn browser_credentials(value: &JsValue) -> Result<GatewayCredentials, String> {
    let credentials = crate::browser_api::js_prop(value, "credentials")?;
    if credentials.is_null() || credentials.is_undefined() {
        return Ok(GatewayCredentials::SameOrigin);
    }
    match credentials
        .as_string()
        .ok_or_else(|| "credentials must be a string".to_string())?
        .as_str()
    {
        "omit" => Ok(GatewayCredentials::Omit),
        "same-origin" => Ok(GatewayCredentials::SameOrigin),
        "include" => Ok(GatewayCredentials::Include),
        value => Err(format!("unsupported request credentials mode {value}")),
    }
}

fn parse_target(value: String, field: &str) -> Result<TargetUrl, String> {
    TargetUrl::parse(value.as_str()).map_err(|error| format!("{field}: {error}"))
}

fn optional_target(value: &JsValue, field: &str) -> Result<Option<TargetUrl>, String> {
    let target = crate::browser_api::js_prop(value, field)?;
    if target.is_null() || target.is_undefined() {
        return Ok(None);
    }
    target
        .as_string()
        .map(|target| parse_target(target, field))
        .transpose()
}

fn browser_headers(value: &JsValue) -> Result<Vec<GatewayHeader>, String> {
    let headers = crate::browser_api::js_prop(value, "headers")?;
    if headers.is_null() || headers.is_undefined() {
        return Ok(Vec::new());
    }
    if !Array::is_array(&headers) {
        return Err("headers is not an array".to_string());
    }
    let headers = Array::from(&headers);
    let mut result = Vec::with_capacity(headers.length() as usize);
    for index in 0..headers.length() {
        let header = headers.get(index);
        let name = crate::browser_api::js_string_field(&header, "name")?;
        let value = crate::browser_api::js_string_field(&header, "value")?;
        result.push(GatewayHeader::new(name, value).map_err(|error| error.to_string())?);
    }
    Ok(result)
}

fn browser_body(value: &JsValue) -> Result<Vec<u8>, String> {
    let body = crate::browser_api::js_prop(value, "body")?;
    if body.is_null() || body.is_undefined() {
        return Ok(Vec::new());
    }
    Ok(Uint8Array::new(&body).to_vec())
}

fn browser_response(response: GatewayResponse) -> JsValue {
    let value = Object::new();
    let headers = Array::new();
    for header in response.headers {
        let entry = Object::new();
        let _name = crate::browser_api::js_set(&entry, "name", &JsValue::from_str(&header.name));
        let _value = crate::browser_api::js_set(&entry, "value", &JsValue::from_str(&header.value));
        headers.push(entry.as_ref());
    }
    let _ok = crate::browser_api::js_set(&value, "ok", &JsValue::TRUE);
    let _status = crate::browser_api::js_set(
        &value,
        "status",
        &JsValue::from_f64(f64::from(response.status)),
    );
    let _headers = crate::browser_api::js_set(&value, "headers", headers.as_ref());
    let body = Uint8Array::from(response.body.as_slice());
    let _body = crate::browser_api::js_set(&value, "body", body.as_ref());
    value.into()
}

fn browser_redirect(location: &str) -> JsValue {
    let value = Object::new();
    let headers = Array::new();
    let header = Object::new();
    let _name = crate::browser_api::js_set(&header, "name", &JsValue::from_str("Location"));
    let _value = crate::browser_api::js_set(&header, "value", &JsValue::from_str(location));
    headers.push(header.as_ref());
    let _ok = crate::browser_api::js_set(&value, "ok", &JsValue::TRUE);
    let _status = crate::browser_api::js_set(&value, "status", &JsValue::from_f64(302.0));
    let _headers = crate::browser_api::js_set(&value, "headers", headers.as_ref());
    value.into()
}

fn browser_failure(status: u16, error: String) -> JsValue {
    browser_failure_with(
        status,
        default_failure_code(status),
        default_failure_summary(status),
        error,
    )
}

fn browser_failure_with(status: u16, code: &str, summary: &str, error: String) -> JsValue {
    let value = Object::new();
    let _ok = crate::browser_api::js_set(&value, "ok", &JsValue::FALSE);
    let _status =
        crate::browser_api::js_set(&value, "status", &JsValue::from_f64(f64::from(status)));
    let _code = crate::browser_api::js_set(&value, "errorCode", &JsValue::from_str(code));
    let _summary = crate::browser_api::js_set(&value, "errorSummary", &JsValue::from_str(summary));
    let _error = crate::browser_api::js_set(&value, "error", &JsValue::from_str(error.as_str()));
    value.into()
}

fn browser_transport_failure(error: WebviewError) -> JsValue {
    match error {
        WebviewError::GatewayFailure(failure) => browser_failure_with(
            failure.status(),
            failure.code(),
            failure.summary(),
            failure.detail().to_string(),
        ),
        WebviewError::Transport(message) => browser_failure_with(
            502,
            "gateway_transport_failed",
            "Gateway transport failed.",
            format!("gateway transport failed: {message}"),
        ),
        other => browser_failure_with(
            502,
            "gateway_transport_failed",
            "Gateway transport failed.",
            format!("gateway transport failed: {other}"),
        ),
    }
}

fn onion_gateway_failure(error: onion::OnionProxyError) -> WebviewError {
    let (status, code, summary) = match error.kind() {
        onion::OnionProxyFailureKind::Generic => {
            (502, "gateway_transport_failed", "Gateway transport failed.")
        }
        onion::OnionProxyFailureKind::ExitUnavailable => (
            503,
            "onion_exit_unavailable",
            "No live HTTPS onion exit is available.",
        ),
        onion::OnionProxyFailureKind::RouteUnavailable => (
            503,
            "onion_route_unavailable",
            "No onion route is currently available for the requested target.",
        ),
        onion::OnionProxyFailureKind::RequestTimedOut => (
            504,
            "onion_request_timed_out",
            "Onion HTTPS proxy request timed out.",
        ),
    };
    WebviewError::GatewayFailure(GatewayFailure::new(
        status,
        code,
        summary,
        format!("gateway transport failed: {}", error.message()),
    ))
}

fn default_failure_code(status: u16) -> &'static str {
    match status {
        400 => "invalid_webview_request",
        403 => "webview_request_rejected",
        404 => "controlled_asset_not_found",
        502 => "gateway_transport_failed",
        503 => "gateway_unavailable",
        _ => "gateway_request_failed",
    }
}

fn default_failure_summary(status: u16) -> &'static str {
    match status {
        400 => "Invalid WebView request.",
        403 => "The WebView gateway rejected this request.",
        404 => "The requested controlled asset was not found.",
        502 => "Gateway transport failed.",
        503 => "Gateway service is unavailable.",
        _ => "Gateway request failed.",
    }
}

/// A browser request captured by the Rings-controlled WebView host.
#[derive(Clone, Debug)]
pub struct WebviewHostRequest {
    requested: TargetUrl,
    source_target: Option<TargetUrl>,
    method: String,
    headers: Vec<GatewayHeader>,
    body: Vec<u8>,
    kind: GatewayRequestKind,
    credentials: GatewayCredentials,
    top_level_navigation: bool,
}

impl WebviewHostRequest {
    /// Build a document navigation request.
    pub fn navigation(requested: TargetUrl) -> Self {
        Self::navigation_with_payload(requested, "GET", Vec::new(), Vec::new())
    }

    /// Build a document navigation request while preserving its HTTP payload.
    pub fn navigation_with_payload(
        requested: TargetUrl,
        method: impl Into<String>,
        headers: Vec<GatewayHeader>,
        body: Vec<u8>,
    ) -> Self {
        Self::new(
            requested,
            None,
            method,
            headers,
            body,
            GatewayRequestKind::Navigation,
        )
        .with_top_level_navigation(true)
    }

    /// Build a static subresource request with its captured HTTP payload.
    pub fn subresource(
        requested: TargetUrl,
        source_target: Option<TargetUrl>,
        method: impl Into<String>,
        headers: Vec<GatewayHeader>,
        body: Vec<u8>,
    ) -> Self {
        Self::new(
            requested,
            source_target,
            method,
            headers,
            body,
            GatewayRequestKind::Subresource,
        )
        .with_top_level_navigation(false)
    }

    /// Build a runtime fetch request with the trusted initiating page target.
    pub fn fetch(
        requested: TargetUrl,
        source_target: TargetUrl,
        method: impl Into<String>,
        headers: Vec<GatewayHeader>,
        body: Vec<u8>,
    ) -> Self {
        Self::runtime(
            requested,
            source_target,
            method,
            headers,
            body,
            GatewayRequestKind::Fetch,
        )
    }

    /// Build a runtime XHR request with the trusted initiating page target.
    pub fn xhr(
        requested: TargetUrl,
        source_target: TargetUrl,
        method: impl Into<String>,
        headers: Vec<GatewayHeader>,
        body: Vec<u8>,
    ) -> Self {
        Self::runtime(
            requested,
            source_target,
            method,
            headers,
            body,
            GatewayRequestKind::Xhr,
        )
    }

    fn new(
        requested: TargetUrl,
        source_target: Option<TargetUrl>,
        method: impl Into<String>,
        headers: Vec<GatewayHeader>,
        body: Vec<u8>,
        kind: GatewayRequestKind,
    ) -> Self {
        Self {
            requested,
            source_target,
            method: method.into(),
            headers,
            body,
            kind,
            credentials: GatewayCredentials::SameOrigin,
            top_level_navigation: kind == GatewayRequestKind::Navigation,
        }
    }

    fn runtime(
        requested: TargetUrl,
        source_target: TargetUrl,
        method: impl Into<String>,
        headers: Vec<GatewayHeader>,
        body: Vec<u8>,
        kind: GatewayRequestKind,
    ) -> Self {
        Self::new(requested, Some(source_target), method, headers, body, kind)
    }

    fn with_credentials(mut self, credentials: GatewayCredentials) -> Self {
        self.credentials = credentials;
        self
    }

    fn with_source_target(mut self, source_target: Option<TargetUrl>) -> Self {
        self.source_target = source_target;
        self
    }

    fn with_top_level_navigation(mut self, top_level_navigation: bool) -> Self {
        self.top_level_navigation = top_level_navigation;
        self
    }

    fn into_gateway_request(self, target: TargetUrl) -> GatewayRequest {
        let source_origin = self.source_target.as_ref().map(target_origin);
        let source_target = self.source_target.map(TargetUrl::into_url);
        GatewayRequest {
            target: target.into_url(),
            method: self.method,
            headers: self.headers,
            body: self.body,
            kind: self.kind,
            source_origin,
            source_target,
            credentials: self.credentials,
            top_level_navigation: self.top_level_navigation,
        }
    }
}

fn target_origin(target: &TargetUrl) -> Url {
    let mut origin = target.as_url().clone();
    let _ = origin.set_username("");
    let _ = origin.set_password(None);
    origin.set_path("/");
    origin.set_query(None);
    origin.set_fragment(None);
    origin
}

/// The only actions a controlled WebView host may take for one captured request.
#[derive(Debug)]
pub enum WebviewHostOutcome {
    /// Load an application-owned asset without proxying it.
    AllowControlled,
    /// Navigate to this controlled gateway URL before any remote connection is opened.
    Redirect(Url),
    /// Return a response that was fetched through the node's onion proxy transport.
    Response(GatewayResponse),
    /// Reject the request without falling back to the requested remote URL.
    Reject(GatewayRouteRejection),
}

/// Per-node WebView gateway state.
///
/// Clones share one cookie jar and a bounded number of concurrent onion requests.
#[derive(Clone)]
pub struct WebviewNode {
    host: Rc<WebviewGatewayHost<OnionGatewayTransport>>,
}

impl WebviewNode {
    /// Attach a WebView gateway when the current page has an HTTP(S) controlled origin.
    pub(crate) fn for_current_window(
        provider: Arc<Provider>,
        onion_settings: WebviewOnionSettings,
    ) -> WebviewResult<Option<Self>> {
        let origin = web_sys::window()
            .ok_or_else(|| WebviewError::Browser("browser window is unavailable".to_string()))?
            .location()
            .origin()
            .map_err(|error| WebviewError::Browser(format!("read frontend origin: {error:?}")))?;
        if !is_http_origin(&origin) {
            return Ok(None);
        }
        let controlled_origin = TargetUrl::parse(&format!("{}/", origin.trim_end_matches('/')))
            .map_err(|error| WebviewError::Browser(format!("parse frontend origin: {error}")))?;
        Self::new(
            Rc::new((*provider).clone()),
            controlled_origin,
            onion_settings,
        )
        .map(Some)
    }

    /// Handle a browser request after the host has captured it before connection dispatch.
    pub async fn handle(&self, request: WebviewHostRequest) -> WebviewResult<WebviewHostOutcome> {
        self.host.handle(request).await
    }

    fn new(
        provider: Rc<Provider>,
        controlled_origin: TargetUrl,
        onion_settings: WebviewOnionSettings,
    ) -> WebviewResult<Self> {
        let prefix = GatewayPrefix::new(GATEWAY_PREFIX)?;
        let policy = GatewayRoutePolicy::new(controlled_origin.into_url(), prefix.clone())?;
        let gateway = ConcurrentWebviewGateway::new(prefix, OnionGatewayTransport {
            provider,
            onion_settings,
        })
        .with_request_bootstrap(webview_bootstrap);
        Ok(Self {
            host: Rc::new(WebviewGatewayHost {
                policy,
                gateway,
                limiter: GatewayRequestLimiter::new(MAX_CONCURRENT_GATEWAY_REQUESTS),
            }),
        })
    }
}

struct WebviewGatewayHost<T> {
    policy: GatewayRoutePolicy,
    gateway: ConcurrentWebviewGateway<T>,
    limiter: GatewayRequestLimiter,
}

impl<T> WebviewGatewayHost<T>
where T: GatewayTransport
{
    async fn handle(&self, request: WebviewHostRequest) -> WebviewResult<WebviewHostOutcome> {
        match self.policy.route(
            request.requested.as_url(),
            request.source_target.as_ref(),
            request.kind,
        )? {
            GatewayRoute::AllowControlled => Ok(WebviewHostOutcome::AllowControlled),
            GatewayRoute::Redirect(location) => Ok(WebviewHostOutcome::Redirect(location)),
            GatewayRoute::Serve(target) => {
                let _permit = self.limiter.acquire().await;
                self.gateway
                    .send(request.into_gateway_request(target))
                    .await
                    .map(WebviewHostOutcome::Response)
            }
            GatewayRoute::Reject(reason) => Ok(WebviewHostOutcome::Reject(reason)),
        }
    }
}

#[derive(Clone)]
struct GatewayRequestLimiter {
    state: Rc<RefCell<GatewayRequestLimiterState>>,
}

struct GatewayRequestLimiterState {
    available: usize,
    next_waiter_id: u64,
    waiters: VecDeque<GatewayRequestWaiter>,
}

struct GatewayRequestWaiter {
    id: u64,
    granted: Rc<Cell<bool>>,
    sender: oneshot::Sender<()>,
}

struct GatewayRequestWaiterGuard {
    limiter: GatewayRequestLimiter,
    id: u64,
    granted: Rc<Cell<bool>>,
    armed: bool,
}

impl GatewayRequestWaiterGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for GatewayRequestWaiterGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.granted.get() {
            self.limiter.release();
        } else {
            self.limiter.cancel_waiter(self.id);
        }
    }
}

impl GatewayRequestLimiter {
    fn new(maximum: usize) -> Self {
        Self {
            state: Rc::new(RefCell::new(GatewayRequestLimiterState {
                available: maximum,
                next_waiter_id: 1,
                waiters: VecDeque::new(),
            })),
        }
    }

    async fn acquire(&self) -> GatewayRequestPermit {
        loop {
            let waiting = {
                let mut state = self.state.borrow_mut();
                if state.available > 0 {
                    state.available -= 1;
                    None
                } else {
                    let id = state.next_waiter_id;
                    state.next_waiter_id = state.next_waiter_id.wrapping_add(1).max(1);
                    let granted = Rc::new(Cell::new(false));
                    let (sender, receiver) = oneshot::channel();
                    state.waiters.push_back(GatewayRequestWaiter {
                        id,
                        granted: granted.clone(),
                        sender,
                    });
                    Some((receiver, GatewayRequestWaiterGuard {
                        limiter: self.clone(),
                        id,
                        granted,
                        armed: true,
                    }))
                }
            };
            let Some((receiver, mut guard)) = waiting else {
                return GatewayRequestPermit {
                    limiter: self.clone(),
                };
            };
            if receiver.await.is_ok() {
                guard.disarm();
                return GatewayRequestPermit {
                    limiter: self.clone(),
                };
            }
        }
    }

    fn cancel_waiter(&self, id: u64) {
        self.state
            .borrow_mut()
            .waiters
            .retain(|waiter| waiter.id != id);
    }

    fn release(&self) {
        let mut state = self.state.borrow_mut();
        while let Some(waiter) = state.waiters.pop_front() {
            waiter.granted.set(true);
            if waiter.sender.send(()).is_ok() {
                return;
            }
            waiter.granted.set(false);
        }
        state.available += 1;
    }
}

struct GatewayRequestPermit {
    limiter: GatewayRequestLimiter,
}

impl Drop for GatewayRequestPermit {
    fn drop(&mut self) {
        self.limiter.release();
    }
}

struct OnionGatewayTransport {
    provider: Rc<Provider>,
    onion_settings: WebviewOnionSettings,
}

#[async_trait(?Send)]
impl GatewayTransport for OnionGatewayTransport {
    async fn send(&self, request: GatewayRequest) -> WebviewResult<GatewayResponse> {
        let options = self.onion_settings.options();
        let should_trace = should_trace_onion_route(request.kind);
        let debug_target = request.target.to_string();
        let debug_source_target = request.source_target.as_ref().map(Url::to_string);
        let debug_kind = request.kind;
        let started = js_sys::Date::now();
        let response_result = onion::request(&self.provider, onion::OnionProxyHttpRequest {
            url: debug_target.clone(),
            method: request.method,
            headers: request
                .headers
                .into_iter()
                .map(|header| (header.name, header.value))
                .collect(),
            body: request.body,
            options,
        })
        .await;
        let duration_ms = (js_sys::Date::now() - started).max(0.0).round();
        let response = match response_result {
            Ok(response) => {
                if should_trace {
                    if let Some(route) = response.route.as_ref() {
                        emit_onion_debug(OnionDebugEvent {
                            message: "route selected",
                            level: "info",
                            target: debug_target.as_str(),
                            source_target: debug_source_target.as_deref(),
                            kind: debug_kind,
                            route: Some(route),
                            error: None,
                            duration_ms,
                        });
                    }
                }
                response
            }
            Err(error) => {
                if should_trace {
                    emit_onion_debug(OnionDebugEvent {
                        message: error.message(),
                        level: "error",
                        target: debug_target.as_str(),
                        source_target: debug_source_target.as_deref(),
                        kind: debug_kind,
                        route: None,
                        error: Some(error.message()),
                        duration_ms,
                    });
                }
                return Err(onion_gateway_failure(error));
            }
        };
        let headers = response
            .headers
            .into_iter()
            .map(|(name, value)| GatewayHeader::new(name, value))
            .collect::<WebviewResult<Vec<_>>>()?;
        GatewayResponse::new(response.status, headers, response.body)
    }
}

fn should_trace_onion_route(kind: GatewayRequestKind) -> bool {
    matches!(
        kind,
        GatewayRequestKind::Navigation | GatewayRequestKind::Fetch | GatewayRequestKind::Xhr
    )
}

struct OnionDebugEvent<'a> {
    message: &'a str,
    level: &'a str,
    target: &'a str,
    source_target: Option<&'a str>,
    kind: GatewayRequestKind,
    route: Option<&'a onion::OnionProxyRoute>,
    error: Option<&'a str>,
    duration_ms: f64,
}

fn emit_onion_debug(event: OnionDebugEvent<'_>) {
    let Ok(bridge) = crate::browser_api::js_global_prop("RingsWebviewHost") else {
        return;
    };
    let Ok(record) = crate::browser_api::js_method(&bridge, "recordDebugEntry") else {
        return;
    };
    let onion = Object::new();
    let _ = crate::browser_api::js_set(&onion, "target", &JsValue::from_str(event.target));
    if let Some(source_target) = event.source_target {
        let _ =
            crate::browser_api::js_set(&onion, "sourceTarget", &JsValue::from_str(source_target));
    }
    let _ = crate::browser_api::js_set(
        &onion,
        "kind",
        &JsValue::from_str(gateway_kind_label(event.kind)),
    );
    let _ = crate::browser_api::js_set(&onion, "durationMs", &JsValue::from_f64(event.duration_ms));
    if let Some(route) = event.route {
        let hops = Array::new();
        for hop in route.hops.iter() {
            hops.push(&JsValue::from_str(hop));
        }
        let _ = crate::browser_api::js_set(&onion, "phase", &JsValue::from_str("selected"));
        let _ = crate::browser_api::js_set(&onion, "service", &JsValue::from_str(&route.service));
        let _ = crate::browser_api::js_set(&onion, "exit", &JsValue::from_str(&route.exit));
        let _ = crate::browser_api::js_set(&onion, "hops", &hops.into());
    } else {
        let _ = crate::browser_api::js_set(&onion, "phase", &JsValue::from_str("failed"));
        let _ = crate::browser_api::js_set(&onion, "service", &JsValue::from_str("https"));
        let _ = crate::browser_api::js_set(&onion, "hops", &Array::new().into());
    }
    if let Some(error) = event.error {
        let _ = crate::browser_api::js_set(&onion, "error", &JsValue::from_str(error));
    }

    let args = Array::new();
    args.push(&JsValue::from_str("onion"));
    args.push(&JsValue::from_str(event.message));
    args.push(&JsValue::from_str(event.level));
    args.push(&JsValue::UNDEFINED);
    args.push(&JsValue::from_bool(true));
    args.push(onion.as_ref());
    let _ = Reflect::apply(&record, &bridge, &args);
}

fn gateway_kind_label(kind: GatewayRequestKind) -> &'static str {
    match kind {
        GatewayRequestKind::Navigation => "navigation",
        GatewayRequestKind::Subresource => "subresource",
        GatewayRequestKind::Fetch => "fetch",
        GatewayRequestKind::Xhr => "xhr",
    }
}

fn webview_bootstrap(request: &GatewayRequest) -> String {
    let bootstrap = bootstrap_script(GATEWAY_PREFIX, &request.target);
    if request.top_level_navigation {
        format!("{bootstrap}\n{WEBVIEW_OVERLAY_LOADER}")
    } else {
        bootstrap
    }
}

fn is_http_origin(origin: &str) -> bool {
    origin.starts_with("http://") || origin.starts_with("https://")
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_arch = "wasm32"))]
mod onion_browser_tests;
