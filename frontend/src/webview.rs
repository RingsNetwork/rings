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
const WEBVIEW_DEBUG_OVERLAY: &str = r#"
(() => {
  "use strict";

  const marker = "__ringsWebviewDebugOverlay";
  if (globalThis[marker]?.mounted?.()) return;

  const state = {
    entries: [],
    log: undefined,
    network: undefined,
    panel: undefined,
    view: "log",
  };
  const maxEntries = 200;

  function resourceLine(resource) {
    const status = resource.status == null ? "pending" : String(resource.status);
    return `#${resource.requestId} ${status} ${resource.kind} ${resource.method} ${resource.phase} ${resource.target} ${resource.durationMs} ms`;
  }

  function render() {
    const container = state.view === "network" ? state.network : state.log;
    if (!container) return;
    container.replaceChildren();
    const entries = state.view === "network"
      ? [...new Map(state.entries.filter((entry) => entry.resource).map((entry) => [entry.resource.requestId, entry])).values()]
      : state.entries;
    if (entries.length === 0) {
      const empty = document.createElement("p");
      empty.textContent = "Waiting for gateway activity";
      container.append(empty);
      return;
    }
    for (const entry of entries) {
      const row = document.createElement("p");
      row.className = entry.level === "error" ? "error" : "";
      const time = String(entry.at || "").split("T")[1]?.replace("Z", "").slice(0, 12) || "";
      row.textContent = state.view === "network" && entry.resource
        ? resourceLine(entry.resource)
        : `${time} [${entry.scope || "worker"}] ${entry.message || "unknown event"}`;
      container.append(row);
    }
    container.scrollTop = container.scrollHeight;
  }

  function record(scope, message, level = "info", resource = undefined) {
    const entry = { at: new Date().toISOString(), scope, message, level };
    if (resource) entry.resource = resource;
    state.entries.push(entry);
    if (state.entries.length > maxEntries) state.entries.splice(0, state.entries.length - maxEntries);
    render();
  }

  function selectView(view) {
    state.view = view;
    state.log.hidden = view !== "log";
    state.network.hidden = view !== "network";
    state.panel.querySelector("[data-view=log]").dataset.active = String(view === "log");
    state.panel.querySelector("[data-view=network]").dataset.active = String(view === "network");
    render();
  }

  function mount() {
    const host = document.createElement("div");
    host.id = "rings-webview-debug-overlay";
    const root = host.attachShadow({ mode: "open" });
    root.innerHTML = `
      <style>
        :host { all: initial; }
        #controls { position: fixed; right: 92px; bottom: 16px; z-index: 2147483647; display: flex; gap: 5px; }
        #controls button { display: grid; width: 34px; height: 34px; min-height: 34px; padding: 0; place-items: center; }
        #toggle { position: fixed; right: 16px; bottom: 16px; z-index: 2147483647; min-width: 68px; min-height: 34px; border: 1px solid #1d2939; border-radius: 5px; background: #111827; color: #fffaf0; font: 700 12px/1 ui-monospace, SFMono-Regular, Menlo, monospace; cursor: pointer; }
        #panel { position: fixed; right: 16px; bottom: 58px; z-index: 2147483647; display: grid; width: min(760px, calc(100vw - 32px)); max-height: min(42vh, 360px); grid-template-rows: auto minmax(0, 1fr); border: 1px solid #1d2939; border-radius: 5px; background: #fffaf0; color: #111827; box-shadow: 0 12px 30px rgba(17, 24, 39, 0.25); overflow: hidden; font: 12px/1.35 ui-monospace, SFMono-Regular, Menlo, monospace; }
        #bar { display: flex; align-items: center; gap: 5px; padding: 7px; border-bottom: 1px solid #d9c5a6; background: #f7eddc; }
        button { min-height: 27px; padding: 4px 8px; border: 1px solid #d9c5a6; border-radius: 4px; background: #fffaf0; color: #374151; font: inherit; font-weight: 700; cursor: pointer; }
        button[data-active=true] { border-color: #111827; background: #111827; color: #fff; }
        #clear { margin-left: auto; }
        .content { min-height: 0; margin: 0; padding: 7px 9px; overflow: auto; background: #fffdf8; }
        p { margin: 0 0 5px; overflow-wrap: anywhere; white-space: pre-wrap; }
        p.error { color: #b42318; }
      </style>
      <div id="controls" role="toolbar" aria-label="WebView navigation">
        <button id="back" type="button" aria-label="Back" title="Back">&lt;</button>
        <button id="forward" type="button" aria-label="Forward" title="Forward">&gt;</button>
        <button id="reload" type="button" aria-label="Reload" title="Reload">&#x21bb;</button>
      </div>
      <button id="toggle" type="button" aria-expanded="false">Debug</button>
      <section id="panel" aria-label="Rings WebView gateway debug log" hidden>
        <div id="bar">
          <button type="button" data-view="log" data-active="true">Logs</button>
          <button type="button" data-view="network" data-active="false">Network</button>
          <button id="clear" type="button">Clear</button>
        </div>
        <div id="log" class="content" role="log" aria-live="polite"></div>
        <div id="network" class="content" role="table" hidden></div>
      </section>`;
    const panel = root.getElementById("panel");
    const toggle = root.getElementById("toggle");
    const back = root.getElementById("back");
    const forward = root.getElementById("forward");
    const reload = root.getElementById("reload");
    const log = root.getElementById("log");
    const network = root.getElementById("network");
    if (!panel || !toggle || !back || !forward || !reload || !log || !network) return;
    state.panel = panel;
    state.log = log;
    state.network = network;
    toggle.addEventListener("click", () => {
      panel.hidden = !panel.hidden;
      toggle.setAttribute("aria-expanded", String(!panel.hidden));
      if (!panel.hidden) render();
    });
    back.addEventListener("click", () => history.back());
    forward.addEventListener("click", () => history.forward());
    reload.addEventListener("click", () => location.reload());
    root.querySelector("[data-view=log]").addEventListener("click", () => selectView("log"));
    root.querySelector("[data-view=network]").addEventListener("click", () => selectView("network"));
    root.getElementById("clear").addEventListener("click", () => {
      state.entries.splice(0, state.entries.length);
      render();
    });
    document.documentElement.append(host);
    record("overlay", "Debug listener ready");
  }

  globalThis[marker] = {
    record,
    mounted: () => Boolean(document.getElementById("rings-webview-debug-overlay")),
  };
  navigator.serviceWorker?.addEventListener("message", (event) => {
    const entry = event.data;
    if (entry?.type === "rings-webview-debug") {
      record(entry.scope || "worker", entry.message || "unknown event", entry.level || "info", entry.resource);
    }
  });
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", mount, { once: true });
  else mount();
  navigator.serviceWorker?.ready.then((registration) => {
    const worker = navigator.serviceWorker.controller || registration.active;
    worker?.postMessage({ type: "rings-webview-debug-register" });
  }).catch(() => record("overlay", "Service Worker debug listener unavailable", "error"));
})();
"#;

thread_local! {
    static BROWSER_GATEWAY: RefCell<Option<BrowserGatewayBinding>> = RefCell::new(None);
}

struct BrowserGatewayBinding {
    // This closure must outlive every Service Worker request sent to the active node.
    _handler: Closure<dyn FnMut(JsValue) -> Promise>,
}

/// Encode an HTTP(S) address into the application-controlled gateway path.
pub(crate) fn controlled_gateway_path(input: &str) -> Result<String, String> {
    let target = TargetUrl::parse(input).map_err(|error| error.to_string())?;
    let prefix = GatewayPrefix::new(GATEWAY_PREFIX).map_err(|error| error.to_string())?;
    Ok(prefix.encode(target.as_url()))
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

/// Ensure that the popup is controlled by the same-origin Service Worker.
pub(crate) async fn prepare_browser_gateway() -> Result<(), String> {
    let bridge = crate::browser_api::js_global_prop("RingsWebviewHost")?;
    let ready = crate::browser_api::js_call0(&bridge, "ensureReady")?;
    let _ready = crate::browser_api::await_js(ready).await?;
    let debug = crate::browser_api::js_call0(&bridge, "enableDebug")?;
    let _debug = crate::browser_api::await_js(debug).await?;
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
        Err(error) => browser_failure(502, format!("gateway transport: {error}")),
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
    let value = Object::new();
    let _ok = crate::browser_api::js_set(&value, "ok", &JsValue::FALSE);
    let _status =
        crate::browser_api::js_set(&value, "status", &JsValue::from_f64(f64::from(status)));
    let _error = crate::browser_api::js_set(&value, "error", &JsValue::from_str(error.as_str()));
    value.into()
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
where
    T: GatewayTransport,
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
        let response = onion::request(
            &self.provider,
            onion::OnionProxyHttpRequest {
                url: request.target.to_string(),
                method: request.method,
                headers: request
                    .headers
                    .into_iter()
                    .map(|header| (header.name, header.value))
                    .collect(),
                body: request.body,
                options: onion::OnionProxyOptions::default(),
            },
        )
        .await
        .map_err(WebviewError::Transport)?;
        let headers = response
            .headers
            .into_iter()
            .map(|(name, value)| GatewayHeader::new(name, value))
            .collect::<WebviewResult<Vec<_>>>()?;
        GatewayResponse::new(response.status, headers, response.body)
    }
}

fn webview_bootstrap(target: &Url) -> String {
    format!(
        "{}\n{WEBVIEW_DEBUG_OVERLAY}",
        bootstrap_script(GATEWAY_PREFIX, target)
    )
}

fn is_http_origin(origin: &str) -> bool {
    origin.starts_with("http://") || origin.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use wasm_bindgen_test::wasm_bindgen_test;

    use super::*;

    type RecordedRequests = Rc<RefCell<Vec<GatewayRequest>>>;
    type FixtureHost = WebviewGatewayHost<FixtureTransport>;

    struct FixtureTransport {
        requests: RecordedRequests,
    }

    impl FixtureTransport {
        fn new(requests: RecordedRequests) -> Self {
            Self { requests }
        }
    }

    #[async_trait(?Send)]
    impl GatewayTransport for FixtureTransport {
        async fn send(&self, request: GatewayRequest) -> WebviewResult<GatewayResponse> {
            self.requests.borrow_mut().push(request);
            let request =
                self.requests.borrow().last().cloned().ok_or_else(|| {
                    WebviewError::Transport("missing fixture request".to_string())
                })?;
            let mut headers = vec![GatewayHeader::new("content-type", "text/html")?];
            if let Some(source) = request.source_origin {
                headers.push(GatewayHeader::new(
                    "access-control-allow-origin",
                    source.origin().ascii_serialization(),
                )?);
            }
            GatewayResponse::new(200, headers, b"<img src=\"/asset.png\">".to_vec())
        }
    }

    fn fixture_host() -> WebviewResult<(FixtureHost, RecordedRequests)> {
        let prefix = GatewayPrefix::new(GATEWAY_PREFIX)?;
        let policy = GatewayRoutePolicy::new(
            TargetUrl::parse("https://frontend.rings.test/")?.into_url(),
            prefix.clone(),
        )?;
        let requests = Rc::new(RefCell::new(Vec::new()));
        let host = WebviewGatewayHost {
            policy,
            gateway: ConcurrentWebviewGateway::new(prefix, FixtureTransport::new(requests.clone()))
                .with_target_bootstrap(webview_bootstrap),
            limiter: GatewayRequestLimiter::new(MAX_CONCURRENT_GATEWAY_REQUESTS),
        };
        Ok((host, requests))
    }

    #[wasm_bindgen_test]
    fn browser_request_allows_source_free_subresources() -> WebviewResult<()> {
        let value = Object::new();
        crate::browser_api::js_set(
            &value,
            "requested",
            &JsValue::from_str("https://static.example.test/site.css"),
        )
        .map_err(WebviewError::Browser)?;
        crate::browser_api::js_set(&value, "method", &JsValue::from_str("GET"))
            .map_err(WebviewError::Browser)?;
        crate::browser_api::js_set(&value, "kind", &JsValue::from_str("subresource"))
            .map_err(WebviewError::Browser)?;

        let request = browser_host_request(&value.into()).map_err(WebviewError::Browser)?;

        assert_eq!(request.kind, GatewayRequestKind::Subresource);
        assert!(request.source_target.is_none());
        Ok(())
    }

    #[wasm_bindgen_test]
    fn host_redirects_then_serves_a_gateway_document_through_its_transport() -> WebviewResult<()> {
        let (host, requests) = fixture_host()?;
        let target = TargetUrl::parse("https://example.test/docs/index.html")?;
        let headers = vec![GatewayHeader::new("accept", "text/html")?];
        let body = vec![0x00, 0xff];
        let redirect =
            futures::executor::block_on(host.handle(WebviewHostRequest::navigation_with_payload(
                target.clone(),
                "POST",
                headers.clone(),
                body.clone(),
            )))?;
        let WebviewHostOutcome::Redirect(gateway_url) = redirect else {
            return Err(WebviewError::Transport(
                "navigation did not redirect to gateway".to_string(),
            ));
        };

        let response =
            futures::executor::block_on(host.handle(WebviewHostRequest::navigation_with_payload(
                TargetUrl::parse(gateway_url.as_str())?,
                "POST",
                headers,
                body,
            )))?;
        let WebviewHostOutcome::Response(response) = response else {
            return Err(WebviewError::Transport(
                "gateway document was not served".to_string(),
            ));
        };
        let body = String::from_utf8(response.body)
            .map_err(|error| WebviewError::Transport(error.to_string()))?;

        assert!(body.contains("data-rings-webview-bootstrap"));
        assert!(body.contains("/webview/https%3A%2F%2Fexample%2Etest%2Fasset%2Epng"));
        assert_eq!(requests.borrow().len(), 1);
        let sent_request = requests
            .borrow()
            .first()
            .cloned()
            .ok_or_else(|| WebviewError::Transport("missing gateway request".to_string()))?;
        assert_eq!(sent_request.target.as_str(), target.as_url().as_str());
        assert_eq!(sent_request.method, "POST");
        assert_eq!(sent_request.body, vec![0x00, 0xff]);
        assert_eq!(
            sent_request.headers,
            vec![GatewayHeader::new("accept", "text/html")?]
        );
        Ok(())
    }

    #[wasm_bindgen_test]
    fn host_serves_cross_target_runtime_reads_when_upstream_allows_cors() -> WebviewResult<()> {
        let (host, requests) = fixture_host()?;
        let source = TargetUrl::parse("https://app.example.test/index.html")?;
        let target = TargetUrl::parse("https://bank.example.test/account")?;

        let outcome = futures::executor::block_on(host.handle(WebviewHostRequest::fetch(
            target,
            source.clone(),
            "GET",
            Vec::new(),
            Vec::new(),
        )))?;

        assert!(matches!(outcome, WebviewHostOutcome::Response(_)));
        let request =
            requests.borrow().first().cloned().ok_or_else(|| {
                WebviewError::Transport("missing cross-origin request".to_string())
            })?;
        assert_eq!(request.source_origin.as_ref(), Some(source.as_url()));
        Ok(())
    }

    #[wasm_bindgen_test]
    fn popup_addresses_only_produce_controlled_gateway_paths() -> WebviewResult<()> {
        let path = controlled_gateway_path("https://example.test/docs/?q=1")
            .map_err(WebviewError::Browser)?;

        assert_eq!(
            path,
            "/webview/https%3A%2F%2Fexample%2Etest%2Fdocs%2F%3Fq%3D1"
        );
        assert!(controlled_gateway_path("file:///tmp/page.html").is_err());
        Ok(())
    }

    #[wasm_bindgen_test]
    fn http_origin_predicate_excludes_extension_and_file_hosts() {
        assert!(is_http_origin("https://frontend.rings.test"));
        assert!(is_http_origin("http://127.0.0.1:8080"));
        assert!(!is_http_origin("chrome-extension://rings"));
        assert!(!is_http_origin("null"));
    }
}
