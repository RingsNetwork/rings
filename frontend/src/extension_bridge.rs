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

const EXTENSION_NODE_BRIDGE: &str = "RingsExtensionNodeBridge";
const NODE_START_POLL_ATTEMPTS: usize = 240;
const NODE_START_POLL_DELAY_MS: u64 = 750;

pub(crate) struct ExtensionStartSettings {
    pub(crate) network_id: String,
    pub(crate) ice_servers: String,
    pub(crate) stabilize_interval: String,
    pub(crate) storage_name: String,
    pub(crate) seed_url: String,
}

pub(crate) struct ExtensionNodeSnapshot {
    pub(crate) online: bool,
    pub(crate) starting: bool,
    pub(crate) did: String,
    pub(crate) peers: Vec<PeerView>,
    pub(crate) wallet_account: Option<WalletAccount>,
    pub(crate) message: String,
    pub(crate) error: Option<String>,
}

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
    }
    status.set(snapshot.error.unwrap_or(snapshot.message));
    true
}

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
    {
        return None;
    }
    Some(bridge)
}

pub(crate) async fn extension_node_start(
    bridge: &JsValue,
    kind: WalletKind,
    settings: ExtensionStartSettings,
) -> Result<ExtensionNodeSnapshot, String> {
    let settings = settings.to_js(kind)?;
    let result = call_extension_bridge1(bridge, "start", &settings).await?;
    parse_extension_node_snapshot(&result, bridge)
}

pub(crate) async fn extension_node_status(
    bridge: &JsValue,
) -> Result<ExtensionNodeSnapshot, String> {
    let result = call_extension_bridge0(bridge, "status").await?;
    parse_extension_node_snapshot(&result, bridge)
}

pub(crate) async fn extension_node_stop(bridge: &JsValue) -> Result<String, String> {
    let result = call_extension_bridge0(bridge, "stop").await?;
    let snapshot = parse_extension_node_snapshot(&result, bridge)?;
    Ok(snapshot.message)
}

pub(crate) async fn extension_node_connect_http(
    bridge: &JsValue,
    endpoint: String,
) -> Result<ExtensionNodeSnapshot, String> {
    let result =
        call_extension_bridge1(bridge, "connectHttp", &JsValue::from_str(&endpoint)).await?;
    parse_extension_node_snapshot(&result, bridge)
}

pub(crate) async fn extension_node_create_offer(
    bridge: &JsValue,
    did: String,
) -> Result<String, String> {
    let result = call_extension_bridge1(bridge, "createOffer", &JsValue::from_str(&did)).await?;
    js_string_field(&result, "offer")
}

pub(crate) async fn extension_node_answer_offer(
    bridge: &JsValue,
    offer: String,
) -> Result<String, String> {
    let result = call_extension_bridge1(bridge, "answerOffer", &JsValue::from_str(&offer)).await?;
    js_string_field(&result, "answer")
}

pub(crate) async fn extension_node_accept_answer(
    bridge: &JsValue,
    answer: String,
) -> Result<ExtensionNodeSnapshot, String> {
    let result =
        call_extension_bridge1(bridge, "acceptAnswer", &JsValue::from_str(&answer)).await?;
    parse_extension_node_snapshot(&result, bridge)
}

pub(crate) async fn extension_onion_proxy_route(
    bridge: &JsValue,
    request: onion::OnionProxyRouteRequest,
) -> Result<onion::OnionProxyRoute, String> {
    let result = call_extension_bridge1(bridge, "onionProxyRoute", &request.to_js()?).await?;
    onion::OnionProxyRoute::from_js(&result)
}

pub(crate) async fn extension_onion_proxy_request(
    bridge: &JsValue,
    request: onion::OnionProxyHttpRequest,
) -> Result<onion::OnionProxyResponse, String> {
    let result = call_extension_bridge1(bridge, "onionProxyRequest", &request.to_js()?).await?;
    onion::OnionProxyResponse::from_js(&result)
}

impl ExtensionStartSettings {
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

async fn call_extension_bridge0(bridge: &JsValue, method: &str) -> Result<JsValue, String> {
    let value = js_method(bridge, method)?
        .call0(bridge)
        .map_err(|error| format!("{method} failed: {}", js_error_label(error)))?;
    await_js(value).await
}

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
