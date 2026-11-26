//! Side-panel client for the MV3 offscreen node bridge.

use std::time::Duration;

use gloo_timers::future::sleep;
use js_sys::Array;
use js_sys::Object;
use js_sys::Reflect;
use wasm_bindgen::JsValue;
use yew::prelude::*;

use crate::browser_api::await_js;
use crate::browser_api::is_callable;
use crate::browser_api::js_bool_field;
use crate::browser_api::js_error_label;
use crate::browser_api::js_method;
use crate::browser_api::js_prop;
use crate::browser_api::js_set;
use crate::browser_api::js_string_field;
use crate::generation::GenerationToken;
use crate::node::PeerView;
use crate::onion;
use crate::wallet::WalletAccount;
use crate::wallet::WalletKind;

/// Global JS property name exposing the MV3 extension node bridge.
const EXTENSION_NODE_BRIDGE: &str = "RingsExtensionNodeBridge";
/// Maximum number of status polls while waiting for background node startup.
const NODE_START_POLL_ATTEMPTS: usize = 240;
/// Delay in milliseconds between background node startup status polls.
const NODE_START_POLL_DELAY_MS: u64 = 750;

/// User-configured runtime settings forwarded to the extension background node on start.
pub(crate) struct ExtensionStartSettings {
    /// Target Rings network identifier string.
    pub(crate) network_id: String,
    /// Semicolon-separated ICE server URLs.
    pub(crate) ice_servers: String,
    /// Stabilization interval in seconds as a decimal string.
    pub(crate) stabilize_interval: String,
    /// IndexedDB storage instance name.
    pub(crate) storage_name: String,
    /// Optional seed node HTTP URL to connect after startup.
    pub(crate) seed_url: String,
}

/// Snapshot of the offscreen background node state returned over the extension bridge.
pub(crate) struct ExtensionNodeSnapshot {
    /// Whether the background node is online and ready.
    pub(crate) online: bool,
    /// Whether a background node startup is currently in progress.
    pub(crate) starting: bool,
    /// Local node DID when online, or empty when offline.
    pub(crate) did: String,
    /// Currently known connected peers.
    pub(crate) peers: Vec<PeerView>,
    /// Active wallet account metadata when online.
    pub(crate) wallet_account: Option<WalletAccount>,
    /// Human-readable status message from the background node.
    pub(crate) message: String,
    /// Startup or runtime error message, if one occurred.
    pub(crate) error: Option<String>,
}

/// Apply an [`ExtensionNodeSnapshot`] to UI state handles if `token` is still current.
pub(crate) fn apply_extension_snapshot(
    snapshot: ExtensionNodeSnapshot,
    did: &UseStateHandle<String>,
    peers: &UseStateHandle<Vec<PeerView>>,
    wallet_account: &UseStateHandle<Option<WalletAccount>>,
    node_starting: &UseStateHandle<bool>,
    status: &UseStateHandle<String>,
    token: &GenerationToken,
) -> bool {
    if !token.is_current() {
        return false;
    }
    node_starting.set(snapshot.starting);
    if snapshot.online {
        did.set(snapshot.did);
        peers.set(snapshot.peers);
        wallet_account.set(snapshot.wallet_account);
    } else {
        did.set(String::new());
        peers.set(Vec::new());
        wallet_account.set(None);
    }
    status.set(snapshot.error.unwrap_or(snapshot.message));
    true
}

/// Poll the extension bridge until the background node finishes starting, fails, or is superseded.
pub(crate) async fn poll_extension_node_start(
    bridge: &JsValue,
    did: UseStateHandle<String>,
    peers: UseStateHandle<Vec<PeerView>>,
    wallet_account: UseStateHandle<Option<WalletAccount>>,
    node_starting: UseStateHandle<bool>,
    status: UseStateHandle<String>,
    token: GenerationToken,
) -> Result<(), String> {
    let mut last_message = "background node starting".to_string();
    for _attempt in 0..NODE_START_POLL_ATTEMPTS {
        sleep(Duration::from_millis(NODE_START_POLL_DELAY_MS)).await;
        if !token.is_current() {
            return Ok(());
        }
        let snapshot = match extension_node_status(bridge).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if token.is_current() {
                    return Err(error);
                }
                return Ok(());
            }
        };
        let message = snapshot
            .error
            .clone()
            .unwrap_or_else(|| snapshot.message.clone());
        last_message = message.clone();
        let online = snapshot.online;
        let starting = snapshot.starting;
        let error = snapshot.error.clone();
        if !apply_extension_snapshot(
            snapshot,
            &did,
            &peers,
            &wallet_account,
            &node_starting,
            &status,
            &token,
        ) {
            return Ok(());
        }
        if online && !starting {
            return Ok(());
        }
        if let Some(error) = error {
            return Err(error);
        }
        if !online && !starting {
            return Err(last_message);
        }
    }
    Err(format!("node start timed out: {last_message}"))
}

/// Return the global `RingsExtensionNodeBridge` JS object when all required bridge methods exist.
pub(crate) fn extension_node_bridge() -> Option<JsValue> {
    let bridge = Reflect::get(&js_sys::global(), &JsValue::from_str(EXTENSION_NODE_BRIDGE)).ok()?;
    if bridge.is_null()
        || bridge.is_undefined()
        || !is_callable(&bridge, "start")
        || !is_callable(&bridge, "stop")
        || !is_callable(&bridge, "status")
        || !is_callable(&bridge, "connectHttp")
        || !is_callable(&bridge, "onionProxyRoute")
        || !is_callable(&bridge, "onionProxyRequest")
        || !is_callable(&bridge, "webviewRequest")
        || !is_callable(&bridge, "openWebview")
    {
        return None;
    }
    Some(bridge)
}

/// Request the extension background node to start with the chosen wallet kind and settings.
pub(crate) async fn extension_node_start(
    bridge: &JsValue,
    kind: WalletKind,
    settings: ExtensionStartSettings,
) -> Result<ExtensionNodeSnapshot, String> {
    let settings = settings.to_js(kind)?;
    let result = call_extension_bridge1(bridge, "start", &settings).await?;
    parse_extension_node_snapshot(&result, bridge)
}

/// Query the current status snapshot from the extension background node.
pub(crate) async fn extension_node_status(
    bridge: &JsValue,
) -> Result<ExtensionNodeSnapshot, String> {
    let result = call_extension_bridge0(bridge, "status").await?;
    parse_extension_node_snapshot(&result, bridge)
}

/// Stop the extension background node and return its resulting status message.
pub(crate) async fn extension_node_stop(bridge: &JsValue) -> Result<String, String> {
    let result = call_extension_bridge0(bridge, "stop").await?;
    let snapshot = parse_extension_node_snapshot(&result, bridge)?;
    Ok(snapshot.message)
}

/// Connect the extension background node to a remote seed HTTP endpoint.
pub(crate) async fn extension_node_connect_http(
    bridge: &JsValue,
    endpoint: String,
) -> Result<ExtensionNodeSnapshot, String> {
    let result =
        call_extension_bridge1(bridge, "connectHttp", &JsValue::from_str(&endpoint)).await?;
    parse_extension_node_snapshot(&result, bridge)
}

/// Create a WebRTC SDP offer for `did` via the extension background node.
pub(crate) async fn extension_node_create_offer(
    bridge: &JsValue,
    did: String,
) -> Result<String, String> {
    let result = call_extension_bridge1(bridge, "createOffer", &JsValue::from_str(&did)).await?;
    js_string_field(&result, "offer")
}

/// Answer an incoming WebRTC SDP offer via the extension background node.
pub(crate) async fn extension_node_answer_offer(
    bridge: &JsValue,
    offer: String,
) -> Result<String, String> {
    let result = call_extension_bridge1(bridge, "answerOffer", &JsValue::from_str(&offer)).await?;
    js_string_field(&result, "answer")
}

/// Accept a remote WebRTC SDP answer via the extension background node.
pub(crate) async fn extension_node_accept_answer(
    bridge: &JsValue,
    answer: String,
) -> Result<ExtensionNodeSnapshot, String> {
    let result =
        call_extension_bridge1(bridge, "acceptAnswer", &JsValue::from_str(&answer)).await?;
    parse_extension_node_snapshot(&result, bridge)
}

/// Resolve an onion proxy route through the extension background node.
pub(crate) async fn extension_onion_proxy_route(
    bridge: &JsValue,
    request: onion::OnionProxyRouteRequest,
) -> Result<onion::OnionProxyRoute, String> {
    let result = call_extension_bridge1(bridge, "onionProxyRoute", &request.to_js()?).await?;
    onion::OnionProxyRoute::from_js(&result)
}

/// Execute an HTTP request over the onion proxy via the extension background node.
pub(crate) async fn extension_onion_proxy_request(
    bridge: &JsValue,
    request: onion::OnionProxyHttpRequest,
) -> Result<onion::OnionProxyResponse, String> {
    let result = call_extension_bridge1(bridge, "onionProxyRequest", &request.to_js()?).await?;
    onion::OnionProxyResponse::from_js(&result)
}

impl ExtensionStartSettings {
    /// Serialize these start settings and `kind` into a JS payload object for the bridge.
    fn to_js(&self, kind: WalletKind) -> Result<JsValue, String> {
        let object = Object::new();
        js_set(&object, "walletKind", &JsValue::from_str(kind.value()))?;
        js_set(&object, "networkId", &JsValue::from_str(&self.network_id))?;
        js_set(&object, "iceServers", &JsValue::from_str(&self.ice_servers))?;
        js_set(
            &object,
            "stabilizeInterval",
            &JsValue::from_str(&self.stabilize_interval),
        )?;
        js_set(
            &object,
            "storageName",
            &JsValue::from_str(&self.storage_name),
        )?;
        js_set(&object, "seedUrl", &JsValue::from_str(&self.seed_url))?;
        Ok(object.into())
    }
}

/// Invoke a zero-argument async method on the JS extension bridge and await its result.
async fn call_extension_bridge0(bridge: &JsValue, method: &str) -> Result<JsValue, String> {
    let value = js_method(bridge, method)?
        .call0(bridge)
        .map_err(|error| format!("{method} failed: {}", js_error_label(error)))?;
    await_js(value).await
}

/// Invoke a single-argument async method on the JS extension bridge and await its result.
async fn call_extension_bridge1(
    bridge: &JsValue,
    method: &str,
    arg: &JsValue,
) -> Result<JsValue, String> {
    let value = js_method(bridge, method)?
        .call1(bridge, arg)
        .map_err(|error| format!("{method} failed: {}", js_error_label(error)))?;
    await_js(value).await
}

/// Parse a JS bridge response object into an [`ExtensionNodeSnapshot`].
fn parse_extension_node_snapshot(
    value: &JsValue,
    bridge: &JsValue,
) -> Result<ExtensionNodeSnapshot, String> {
    let online = js_bool_field(value, "online").unwrap_or(false);
    let starting = js_bool_field(value, "starting").unwrap_or(false);
    let did = js_string_field(value, "did").unwrap_or_default();
    let message = js_string_field(value, "message").unwrap_or_else(|_| {
        if online {
            "background node active".to_string()
        } else {
            "background node offline".to_string()
        }
    });
    let error = js_string_field(value, "error").ok();
    let peers = parse_peer_views(value)?;
    let wallet_account = if online {
        let account = js_string_field(value, "account").unwrap_or_default();
        if account.is_empty() {
            None
        } else {
            let kind = js_string_field(value, "walletKind")
                .map(|value| WalletKind::from_value(&value))
                .unwrap_or(WalletKind::WebCrypto);
            let account_type =
                js_string_field(value, "accountType").unwrap_or_else(|_| "unknown".to_string());
            Some(WalletAccount::extension_view(
                kind,
                account,
                account_type,
                bridge.clone(),
            ))
        }
    } else {
        None
    };
    Ok(ExtensionNodeSnapshot {
        online,
        starting,
        did,
        peers,
        wallet_account,
        message,
        error,
    })
}

/// Parse the `peers` JS array field of a bridge snapshot into [`PeerView`] entries.
fn parse_peer_views(value: &JsValue) -> Result<Vec<PeerView>, String> {
    let peers = js_prop(value, "peers").unwrap_or_else(|_| Array::new().into());
    if !Array::is_array(&peers) {
        return Ok(Vec::new());
    }
    let array = Array::from(&peers);
    let mut out = Vec::with_capacity(array.length() as usize);
    for index in 0..array.length() {
        let peer = array.get(index);
        let did = js_string_field(&peer, "did").unwrap_or_default();
        let state = js_string_field(&peer, "state").unwrap_or_else(|_| "Unknown".to_string());
        if let Some(peer) = PeerView::from_fields(did, state) {
            out.push(peer);
        }
    }
    Ok(out)
}
