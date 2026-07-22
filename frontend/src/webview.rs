//! Frontend host adapter for the reusable Rings webview gateway.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use futures::channel::oneshot;
use js_sys::Array;
use js_sys::Object;
use js_sys::Promise;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_node::provider::Provider;
use rings_webview::browser::bootstrap_script;
use rings_webview::ConcurrentWebviewGateway;
use rings_webview::GatewayFailure;
use rings_webview::GatewayCredentials;
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
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::future_to_promise;

use crate::onion;

const GATEWAY_PREFIX: &str = "/webview/";
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

  const script = document.createElement("script");
  script.src = "/assets/webview-overlay.js";
  script.async = false;
  script.dataset.ringsWebviewOverlayLoader = "";
  (document.head || document.documentElement).append(script);
})();
"#;

thread_local! {
    static BROWSER_GATEWAY: RefCell<Option<BrowserGatewayBinding>> = RefCell::new(None);
}

struct BrowserGatewayBinding {
    // This closure must outlive every Service Worker request sent to the active node.
    _handler: Closure<dyn FnMut(JsValue) -> Promise>,
}

/// Open the local WebView popup without ever passing it a remote URL.
pub(crate) fn open_webview_popup() -> Result<(), String> {
    crate::browser_api::open_webview_popup()
}

/// Install the current node as the only Service Worker gateway executor.
pub(crate) fn install_browser_gateway(gateway: Option<WebviewNode>) -> Result<bool, String> {
    clear_browser_gateway();
    let Some(gateway) = gateway else {
        return Ok(false);
    };
    let handler = Closure::wrap(Box::new(move |request: JsValue| {
        let gateway = gateway.clone();
        future_to_promise(async move {
            Ok::<JsValue, JsValue>(dispatch_browser_request(gateway, request).await)
        })
    }) as Box<dyn FnMut(JsValue) -> Promise>);
    let bridge = Object::new();
    crate::browser_api::js_set(&bridge, "handle", handler.as_ref())?;
    Reflect::set(
        &js_sys::global(),
        &JsValue::from_str("RingsWebviewGateway"),
        bridge.as_ref(),
    )
    .map_err(crate::browser_api::js_error_label)?;
    BROWSER_GATEWAY.with(|slot| {
        *slot.borrow_mut() = Some(BrowserGatewayBinding { _handler: handler });
    });
    Ok(true)
}

/// Remove the browser host binding when the local node stops.
pub(crate) fn clear_browser_gateway() {
    BROWSER_GATEWAY.with(|slot| {
        *slot.borrow_mut() = None;
    });
    let _deleted =
        Reflect::delete_property(&js_sys::global(), &JsValue::from_str("RingsWebviewGateway"));
}

/// Register the active application page as the Service Worker's gateway host.
pub(crate) async fn register_browser_gateway() -> Result<(), String> {
    let bridge = crate::browser_api::js_global_prop("RingsWebviewHost")?;
    let registered = crate::browser_api::js_call0(&bridge, "registerGatewayHost")?;
    let _ready = crate::browser_api::await_js(registered).await?;
    Ok(())
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
    let request = match crate::browser_api::js_string_field(value, "kind")?.as_str() {
        "navigation" => Ok(WebviewHostRequest::navigation_with_payload(
            requested, method, headers, body,
        )),
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

fn browser_failure_with(
    status: u16,
    code: &str,
    summary: &str,
    error: String,
) -> JsValue {
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
            BrowserFailureKind::GatewayTransport.status(),
            BrowserFailureKind::GatewayTransport.code(),
            BrowserFailureKind::GatewayTransport.summary(),
            format!("gateway transport failed: {message}"),
        ),
        other => browser_failure_with(
            BrowserFailureKind::GatewayTransport.status(),
            BrowserFailureKind::GatewayTransport.code(),
            BrowserFailureKind::GatewayTransport.summary(),
            format!("gateway transport failed: {other}"),
        ),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowserFailureKind {
    GatewayTransport,
    OnionExitUnavailable,
    OnionRouteUnavailable,
    OnionRequestTimedOut,
}

impl BrowserFailureKind {
    fn status(self) -> u16 {
        match self {
            Self::GatewayTransport => 502,
            Self::OnionExitUnavailable | Self::OnionRouteUnavailable => 503,
            Self::OnionRequestTimedOut => 504,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::GatewayTransport => "gateway_transport_failed",
            Self::OnionExitUnavailable => "onion_exit_unavailable",
            Self::OnionRouteUnavailable => "onion_route_unavailable",
            Self::OnionRequestTimedOut => "onion_request_timed_out",
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Self::GatewayTransport => "Gateway transport failed.",
            Self::OnionExitUnavailable => "No live HTTPS onion exit is available.",
            Self::OnionRouteUnavailable => {
                "No onion route is currently available for the requested target."
            }
            Self::OnionRequestTimedOut => "Onion HTTPS proxy request timed out.",
        }
    }
}

fn onion_gateway_failure(error: onion::OnionProxyError) -> WebviewError {
    let failure = match error.kind() {
        onion::OnionProxyFailureKind::Generic => BrowserFailureKind::GatewayTransport,
        onion::OnionProxyFailureKind::ExitUnavailable => BrowserFailureKind::OnionExitUnavailable,
        onion::OnionProxyFailureKind::RouteUnavailable => BrowserFailureKind::OnionRouteUnavailable,
        onion::OnionProxyFailureKind::RequestTimedOut => BrowserFailureKind::OnionRequestTimedOut,
    };
    WebviewError::GatewayFailure(GatewayFailure::new(
        failure.status(),
        failure.code(),
        failure.summary(),
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

    fn into_gateway_request(self, target: TargetUrl) -> GatewayRequest {
        let source_origin = matches!(
            self.kind,
            GatewayRequestKind::Fetch | GatewayRequestKind::Xhr
        )
        .then(|| {
            self.source_target
                .as_ref()
                .map(|source| source.as_url().clone())
        })
        .flatten();
        GatewayRequest {
            target: target.into_url(),
            method: self.method,
            headers: self.headers,
            body: self.body,
            kind: self.kind,
            source_origin,
            credentials: self.credentials,
        }
    }
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
    pub fn for_current_window(provider: Arc<Provider>) -> WebviewResult<Option<Self>> {
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
        Self::new(provider, controlled_origin).map(Some)
    }

    /// Handle a browser request after the host has captured it before connection dispatch.
    pub async fn handle(&self, request: WebviewHostRequest) -> WebviewResult<WebviewHostOutcome> {
        self.host.handle(request).await
    }

    fn new(provider: Arc<Provider>, controlled_origin: TargetUrl) -> WebviewResult<Self> {
        let prefix = GatewayPrefix::new(GATEWAY_PREFIX)?;
        let policy = GatewayRoutePolicy::new(controlled_origin.into_url(), prefix.clone())?;
        let gateway = ConcurrentWebviewGateway::new(prefix, OnionGatewayTransport { provider })
            .with_target_bootstrap(webview_bootstrap);
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
    waiters: VecDeque<oneshot::Sender<()>>,
}

impl GatewayRequestLimiter {
    fn new(maximum: usize) -> Self {
        Self {
            state: Rc::new(RefCell::new(GatewayRequestLimiterState {
                available: maximum,
                waiters: VecDeque::new(),
            })),
        }
    }

    async fn acquire(&self) -> GatewayRequestPermit {
        let receiver = {
            let mut state = self.state.borrow_mut();
            if state.available > 0 {
                state.available -= 1;
                None
            } else {
                let (sender, receiver) = oneshot::channel();
                state.waiters.push_back(sender);
                Some(receiver)
            }
        };
        if let Some(receiver) = receiver {
            let _ = receiver.await;
        }
        GatewayRequestPermit {
            limiter: self.clone(),
        }
    }

    fn release(&self) {
        let mut state = self.state.borrow_mut();
        while let Some(waiter) = state.waiters.pop_front() {
            if waiter.send(()).is_ok() {
                return;
            }
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
    provider: Arc<Provider>,
}

#[async_trait(?Send)]
impl GatewayTransport for OnionGatewayTransport {
    async fn send(&self, request: GatewayRequest) -> WebviewResult<GatewayResponse> {
        let options = onion::OnionProxyOptions::default();
        if should_trace_onion_route(request.kind) {
            trace_onion_route(
                &self.provider,
                request.target.as_str(),
                request.kind,
                options,
            )
            .await;
        }
        let response = onion::request(&self.provider, onion::OnionProxyHttpRequest {
            url: request.target.to_string(),
            method: request.method,
            headers: request
                .headers
                .into_iter()
                .map(|header| (header.name, header.value))
                .collect(),
            body: request.body,
            options,
        })
        .await
        .map_err(onion_gateway_failure)?;
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

async fn trace_onion_route(
    provider: &Arc<Provider>,
    target: &str,
    kind: GatewayRequestKind,
    options: onion::OnionProxyOptions,
) {
    let started = js_sys::Date::now();
    let result = onion::route(provider, onion::OnionProxyRouteRequest {
        url: target.to_string(),
        options,
    })
    .await;
    let duration = (js_sys::Date::now() - started).max(0.0).round();
    match result {
        Ok(route) => emit_onion_debug(
            "route selected",
            "info",
            target,
            kind,
            Some(&route),
            None,
            duration,
        ),
        Err(error) => emit_onion_debug(
            error.message(),
            "error",
            target,
            kind,
            None,
            Some(error.message()),
            duration,
        ),
    }
}

fn emit_onion_debug(
    message: &str,
    level: &str,
    target: &str,
    kind: GatewayRequestKind,
    route: Option<&onion::OnionProxyRoute>,
    error: Option<&str>,
    duration_ms: f64,
) {
    let Ok(bridge) = crate::browser_api::js_global_prop("RingsWebviewHost") else {
        return;
    };
    let Ok(record) = crate::browser_api::js_method(&bridge, "recordDebugEntry") else {
        return;
    };
    let onion = Object::new();
    let _ = crate::browser_api::js_set(&onion, "target", &JsValue::from_str(target));
    let _ =
        crate::browser_api::js_set(&onion, "kind", &JsValue::from_str(gateway_kind_label(kind)));
    let _ = crate::browser_api::js_set(&onion, "durationMs", &JsValue::from_f64(duration_ms));
    if let Some(route) = route {
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
    if let Some(error) = error {
        let _ = crate::browser_api::js_set(&onion, "error", &JsValue::from_str(error));
    }

    let args = Array::new();
    args.push(&JsValue::from_str("onion"));
    args.push(&JsValue::from_str(message));
    args.push(&JsValue::from_str(level));
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

fn webview_bootstrap(target: &Url) -> String {
    format!(
        "{}\n{WEBVIEW_OVERLAY_LOADER}",
        bootstrap_script(GATEWAY_PREFIX, target)
    )
}

fn is_http_origin(origin: &str) -> bool {
    origin.starts_with("http://") || origin.starts_with("https://")
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_arch = "wasm32"))]
mod onion_browser_tests;
