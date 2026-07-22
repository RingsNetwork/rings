//! Browser-extension runtime and side-panel bridge boundary.

use std::cell::RefCell;
use std::fmt;
use std::future::Future;
use std::num::ParseIntError;
use std::rc::Rc;
use std::time::Duration;

use futures::FutureExt;
use gloo_timers::future::sleep;
use js_sys::Array;
use js_sys::Function;
use js_sys::Object;
use js_sys::Reflect;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use yew::prelude::*;

use crate::browser_api::chrome_runtime_on_message;
pub(crate) use crate::browser_api::copy_text_to_clipboard;
use crate::browser_api::js_method;
use crate::browser_api::js_prop;
use crate::browser_api::js_set;
use crate::browser_api::js_string_field;
pub(crate) use crate::browser_api::load_setting;
pub(crate) use crate::browser_api::open_debug_url;
pub(crate) use crate::browser_api::save_setting;
pub(crate) use crate::browser_api::DebugUrlOpenResult;
use crate::custom;
use crate::dweb;
pub(crate) use crate::extension_bridge::apply_extension_snapshot;
pub(crate) use crate::extension_bridge::extension_node_accept_answer;
pub(crate) use crate::extension_bridge::extension_node_answer_offer;
pub(crate) use crate::extension_bridge::extension_node_bridge;
pub(crate) use crate::extension_bridge::extension_node_connect_http;
pub(crate) use crate::extension_bridge::extension_node_create_offer;
pub(crate) use crate::extension_bridge::extension_node_start;
pub(crate) use crate::extension_bridge::extension_node_status;
pub(crate) use crate::extension_bridge::extension_node_stop;
pub(crate) use crate::extension_bridge::extension_onion_proxy_request;
pub(crate) use crate::extension_bridge::extension_onion_proxy_route;
pub(crate) use crate::extension_bridge::poll_extension_node_start;
pub(crate) use crate::extension_bridge::ExtensionNodeSnapshot;
pub(crate) use crate::extension_bridge::ExtensionStartSettings;
use crate::node;
use crate::node::DemoNode;
use crate::node::NodeSettings;
use crate::node::PeerView;
use crate::onion;
use crate::peer_sync;
use crate::wallet;
use crate::wallet::WalletAccount;
use crate::wallet::WalletKind;

const EXTENSION_NODE_TARGET: &str = "rings.node.offscreen";
const EXTENSION_NODE_START: &str = "rings.node.start";
const EXTENSION_NODE_STOP: &str = "rings.node.stop";
const EXTENSION_NODE_STATUS: &str = "rings.node.status";
const EXTENSION_NODE_CONNECT_HTTP: &str = "rings.node.connectHttp";
const EXTENSION_NODE_CREATE_OFFER: &str = "rings.node.createOffer";
const EXTENSION_NODE_ANSWER_OFFER: &str = "rings.node.answerOffer";
const EXTENSION_NODE_ACCEPT_ANSWER: &str = "rings.node.acceptAnswer";
const EXTENSION_NODE_ONION_ROUTE: &str = "rings.node.onionProxyRoute";
const EXTENSION_NODE_ONION_REQUEST: &str = "rings.node.onionProxyRequest";
pub(crate) const SETTING_WALLET_KIND: &str = "rings.frontend.walletKind";
pub(crate) const SETTING_NETWORK_ID: &str = "rings.frontend.networkId";
pub(crate) const SETTING_ICE_SERVERS: &str = "rings.frontend.iceServers";
pub(crate) const SETTING_STABILIZE_INTERVAL: &str = "rings.frontend.stabilizeInterval";
pub(crate) const SETTING_STORAGE_NAME: &str = "rings.frontend.storageName";
pub(crate) const SETTING_SEED_URL: &str = "rings.frontend.seedUrl";
pub(crate) const SETTING_HTTP_ENDPOINT: &str = "rings.frontend.httpEndpoint";

// Preserve settings saved before the browser demo was renamed to frontend.
pub(crate) const LEGACY_SETTING_WALLET_KIND: &str = "rings.node-demo.walletKind";
pub(crate) const LEGACY_SETTING_NETWORK_ID: &str = "rings.node-demo.networkId";
pub(crate) const LEGACY_SETTING_ICE_SERVERS: &str = "rings.node-demo.iceServers";
pub(crate) const LEGACY_SETTING_STABILIZE_INTERVAL: &str = "rings.node-demo.stabilizeInterval";
pub(crate) const LEGACY_SETTING_STORAGE_NAME: &str = "rings.node-demo.storageName";
pub(crate) const LEGACY_SETTING_SEED_URL: &str = "rings.node-demo.seedUrl";
pub(crate) const LEGACY_SETTING_HTTP_ENDPOINT: &str = "rings.node-demo.httpEndpoint";
pub(crate) const WALLET_CONNECT_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const SESSION_AUTH_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn load_setting_with_legacy(key: &str, legacy_key: &str) -> Option<String> {
    load_setting(key).or_else(|| {
        let value = load_setting(legacy_key)?;
        save_setting(key, &value);
        Some(value)
    })
}

struct HeadlessNodeState {
    node: Option<DemoNode>,
    wallet_account: Option<WalletAccount>,
    peers: Vec<PeerView>,
    starting: bool,
    start_error: Option<String>,
    message: String,
    generation: u64,
}

struct HeadlessDemoNode {
    node: DemoNode,
    generation: u64,
}

/// Return true when this wasm bundle is mounted inside the MV3 offscreen page.
pub fn is_offscreen_document() -> bool {
    Reflect::get(&js_sys::global(), &JsValue::from_str("location"))
        .ok()
        .and_then(|location| js_string_field(&location, "pathname").ok())
        .is_some_and(|pathname| pathname.ends_with("offscreen.html"))
}

/// Headless MV3 offscreen node. It owns the browser node while the side panel is closed.
#[function_component(HeadlessNode)]
pub fn headless_node() -> Html {
    let state = use_mut_ref(|| HeadlessNodeState {
        node: None,
        wallet_account: None,
        peers: Vec::new(),
        starting: false,
        start_error: None,
        message: "background node offline".to_string(),
        generation: 0,
    });

    {
        let state = state.clone();
        use_effect_with((), move |_| {
            let Some(on_message) = chrome_runtime_on_message() else {
                return Box::new(|| {}) as Box<dyn FnOnce()>;
            };
            let Some(add_listener) = js_method(&on_message, "addListener").ok() else {
                return Box::new(|| {}) as Box<dyn FnOnce()>;
            };
            let remove_listener = js_method(&on_message, "removeListener").ok();
            let listener = Closure::<dyn FnMut(JsValue, JsValue, Function) -> bool>::new({
                let state = state.clone();
                move |message: JsValue, _sender: JsValue, send_response: Function| {
                    let target = js_string_field(&message, "target").unwrap_or_default();
                    if target != EXTENSION_NODE_TARGET {
                        return false;
                    }
                    let message_type = js_string_field(&message, "type").unwrap_or_default();
                    if message_type.is_empty() {
                        send_node_response(
                            send_response,
                            Err("missing node message type".to_string()),
                        );
                        return false;
                    }
                    let state = state.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        let response =
                            handle_headless_node_message(state, message_type, message).await;
                        send_node_response(send_response, response);
                    });
                    true
                }
            });
            let listener_ref: &Function = listener.as_ref().unchecked_ref();
            let _added = add_listener.call1(&on_message, listener_ref);
            Box::new(move || {
                if let Some(remove_listener) = remove_listener {
                    let listener_ref: &Function = listener.as_ref().unchecked_ref();
                    let _removed = remove_listener.call1(&on_message, listener_ref);
                }
            }) as Box<dyn FnOnce()>
        });
    }

    html! {}
}

pub(crate) fn node_settings(
    network_id: String,
    ice_servers: String,
    stabilize_interval: String,
    storage_name: String,
) -> Result<NodeSettings, NodeSettingsError> {
    let network_id = network_id
        .trim()
        .parse::<u32>()
        .map_err(NodeSettingsError::NetworkId)?;
    let stabilize_interval = stabilize_interval
        .trim()
        .parse::<u64>()
        .map_err(NodeSettingsError::StabilizeInterval)?;
    Ok(NodeSettings {
        network_id,
        ice_servers,
        stabilize_interval,
        storage_name,
    })
}

#[derive(Debug)]
pub(crate) enum NodeSettingsError {
    NetworkId(ParseIntError),
    StabilizeInterval(ParseIntError),
}

impl fmt::Display for NodeSettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NetworkId(error) => write!(formatter, "invalid network id: {error}"),
            Self::StabilizeInterval(error) => {
                write!(formatter, "invalid stabilize interval: {error}")
            }
        }
    }
}

impl std::error::Error for NodeSettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NetworkId(error) | Self::StabilizeInterval(error) => Some(error),
        }
    }
}

async fn handle_headless_node_message(
    state: Rc<RefCell<HeadlessNodeState>>,
    message_type: String,
    message: JsValue,
) -> Result<JsValue, String> {
    match message_type.as_str() {
        EXTENSION_NODE_STATUS => {
            headless_node_snapshot(
                state,
                "background node active".to_string(),
                None,
                false,
                None,
            )
            .await
        }
        EXTENSION_NODE_START => start_headless_node(state, &message).await,
        EXTENSION_NODE_STOP => stop_headless_node(state).await,
        EXTENSION_NODE_CONNECT_HTTP => connect_headless_node_http(state, &message).await,
        EXTENSION_NODE_CREATE_OFFER => create_headless_offer(state, &message).await,
        EXTENSION_NODE_ANSWER_OFFER => answer_headless_offer(state, &message).await,
        EXTENSION_NODE_ACCEPT_ANSWER => accept_headless_answer(state, &message).await,
        EXTENSION_NODE_ONION_ROUTE => route_headless_onion_proxy(state, &message).await,
        EXTENSION_NODE_ONION_REQUEST => request_headless_onion_proxy(state, &message).await,
        _ => Err(format!("unknown node message type {message_type}")),
    }
}

async fn start_headless_node(
    state: Rc<RefCell<HeadlessNodeState>>,
    message: &JsValue,
) -> Result<JsValue, String> {
    if state.borrow().node.is_some() {
        return headless_node_snapshot(
            state,
            "background node already active".to_string(),
            None,
            false,
            None,
        )
        .await;
    }
    if state.borrow().starting {
        return headless_node_snapshot(
            state,
            "background node starting".to_string(),
            None,
            false,
            None,
        )
        .await;
    }

    let settings_value = js_prop(message, "settings")?;
    let wallet_kind = js_string_field(&settings_value, "walletKind")
        .map(|value| WalletKind::from_value(&value))
        .unwrap_or(WalletKind::WebCrypto);
    let settings = extension_start_settings_from_js(&settings_value);
    let generation = begin_headless_start(&state, format!("connecting {}", wallet_kind.label()));
    wasm_bindgen_futures::spawn_local(run_headless_node_start(
        state.clone(),
        generation,
        wallet_kind,
        settings,
    ));
    headless_node_snapshot(
        state,
        "background node starting".to_string(),
        None,
        false,
        Some(generation),
    )
    .await
}

async fn run_headless_node_start(
    state: Rc<RefCell<HeadlessNodeState>>,
    generation: u64,
    wallet_kind: WalletKind,
    settings: ExtensionStartSettings,
) {
    match start_headless_node_inner(state.clone(), generation, wallet_kind, settings).await {
        Ok(message) => {
            set_headless_starting_for_generation(&state, generation, message, None, false);
        }
        Err(error) => {
            set_headless_starting_for_generation(
                &state,
                generation,
                error.clone(),
                Some(error),
                false,
            );
        }
    }
}

async fn start_headless_node_inner(
    state: Rc<RefCell<HeadlessNodeState>>,
    generation: u64,
    wallet_kind: WalletKind,
    settings: ExtensionStartSettings,
) -> Result<String, String> {
    let node_settings = headless_node_settings(&settings)?;
    let account = authorize_headless_account(wallet_kind).await?;
    if !headless_generation_current(&state, generation) {
        return Ok("background node start cancelled".to_string());
    }
    set_headless_starting_for_generation(
        &state,
        generation,
        "authorizing session key".to_string(),
        None,
        true,
    );
    let built = build_headless_node(&account, node_settings).await?;
    if !headless_generation_current(&state, generation) {
        built.stop();
        return Ok("background node start cancelled".to_string());
    }
    set_headless_starting_for_generation(
        &state,
        generation,
        "registering node protocols".to_string(),
        None,
        true,
    );
    let my_did = built.provider.address();
    if let Err(error) = register_headless_protocols(&built, &my_did) {
        built.stop();
        return Err(error);
    }
    if !headless_generation_current(&state, generation) {
        built.stop();
        return Ok("background node start cancelled".to_string());
    }

    install_headless_node(&state, account, &built);
    connect_headless_seed(state, generation, built, settings.seed_url).await
}

fn headless_node_settings(settings: &ExtensionStartSettings) -> Result<NodeSettings, String> {
    node_settings(
        settings.network_id.clone(),
        settings.ice_servers.clone(),
        settings.stabilize_interval.clone(),
        settings.storage_name.clone(),
    )
    .map_err(|error| error.to_string())
}

async fn authorize_headless_account(wallet_kind: WalletKind) -> Result<WalletAccount, String> {
    operation_timeout(
        "account authorization",
        WALLET_CONNECT_TIMEOUT,
        wallet::connect(wallet_kind),
    )
    .await
}

async fn build_headless_node(
    account: &WalletAccount,
    node_settings: NodeSettings,
) -> Result<DemoNode, String> {
    operation_timeout(
        "session authorization",
        SESSION_AUTH_TIMEOUT,
        node::build_node(account, node_settings),
    )
    .await
}

fn register_headless_protocols(built: &DemoNode, my_did: &str) -> Result<(), String> {
    let site = Rc::new(RefCell::new(dweb::default_site()));
    site.borrow_mut().insert(
        "/".to_string(),
        format!("<h1>Rings node {my_did}</h1><p>Served by the extension background node.</p>"),
    );
    dweb::register(
        &built.provider,
        site,
        Callback::from(|_: dweb::DwebResponse| {}),
    )?;
    let on_custom = Callback::from(|_: custom::CustomEvent| {});
    for namespace in custom::DEMO_NAMESPACES {
        custom::register(&built.provider, namespace.to_string(), on_custom.clone())?;
    }
    Ok(())
}

fn install_headless_node(
    state: &Rc<RefCell<HeadlessNodeState>>,
    account: WalletAccount,
    built: &DemoNode,
) {
    let mut state = state.borrow_mut();
    state.wallet_account = Some(account);
    state.peers = Vec::new();
    state.node = Some(built.clone());
}

async fn connect_headless_seed(
    state: Rc<RefCell<HeadlessNodeState>>,
    generation: u64,
    built: DemoNode,
    seed_url: String,
) -> Result<String, String> {
    let seed_url = seed_url.trim().to_string();
    if seed_url.is_empty() {
        return Ok("background node ready".to_string());
    }

    set_headless_starting_for_generation(
        &state,
        generation,
        format!("background node ready; connecting seed {seed_url}"),
        None,
        true,
    );
    match node::connect_http(&built.provider, seed_url).await {
        Ok(seed_did) => {
            let seed_peer = PeerView::connected(seed_did)
                .ok_or_else(|| "seed returned an empty DID".to_string())?;
            let snapshot = headless_node_snapshot(
                state.clone(),
                "seed URL connected".to_string(),
                Some(seed_peer),
                true,
                Some(generation),
            )
            .await?;
            Ok(js_string_field(&snapshot, "message")
                .unwrap_or_else(|_| "seed URL connected".to_string()))
        }
        Err(error) => Ok(format!(
            "background node ready; seed connect failed: {error}"
        )),
    }
}

async fn stop_headless_node(state: Rc<RefCell<HeadlessNodeState>>) -> Result<JsValue, String> {
    let (node, stop_generation) = {
        let mut state = state.borrow_mut();
        state.generation = state.generation.wrapping_add(1);
        state.wallet_account = None;
        state.peers = Vec::new();
        state.starting = false;
        state.start_error = None;
        state.message = "stopping background node".to_string();
        let node = state.node.take();
        (node, state.generation)
    };
    let Some(node) = node else {
        let message = "background node already offline".to_string();
        state.borrow_mut().message = message.clone();
        return headless_snapshot_js(false, String::new(), &[], None, message, false, None);
    };

    let provider = node.provider.clone();
    let cleanup = node::disconnect_all(&provider).fuse();
    let timeout = sleep(Duration::from_secs(2)).fuse();
    futures::pin_mut!(cleanup, timeout);
    let message = futures::select! {
        result = cleanup => match result {
            Ok(0) => "background node stopped".to_string(),
            Ok(count) => format!("background node stopped; closed {count} peer links"),
            Err(error) => format!("background node stopped; peer cleanup failed: {error}"),
        },
        _ = timeout => "background node stopped; peer cleanup timed out".to_string(),
    };
    node.stop();
    {
        let mut state = state.borrow_mut();
        if state.generation == stop_generation {
            state.message = message.clone();
        }
    }
    headless_snapshot_js(false, String::new(), &[], None, message, false, None)
}

async fn connect_headless_node_http(
    state: Rc<RefCell<HeadlessNodeState>>,
    message: &JsValue,
) -> Result<JsValue, String> {
    let handle = headless_demo_node(&state)?;
    let endpoint = required_message_field(message, "endpoint", "enter a seed HTTP endpoint")?;
    let seed_did = node::connect_http(&handle.node.provider, endpoint).await?;
    let seed_peer =
        PeerView::connected(seed_did).ok_or_else(|| "seed returned an empty DID".to_string())?;
    headless_node_snapshot(
        state,
        "HTTP endpoint connected".to_string(),
        Some(seed_peer),
        true,
        Some(handle.generation),
    )
    .await
}

async fn create_headless_offer(
    state: Rc<RefCell<HeadlessNodeState>>,
    message: &JsValue,
) -> Result<JsValue, String> {
    let handle = headless_demo_node(&state)?;
    let remote_did = required_message_field(message, "did", "enter a remote DID")?;
    let offer = node::create_offer(&handle.node.provider, remote_did).await?;
    ensure_headless_generation_current(&state, handle.generation)?;
    let result = Object::new();
    js_set(&result, "offer", &JsValue::from_str(&offer))?;
    js_set(&result, "message", &JsValue::from_str("offer created"))?;
    Ok(result.into())
}

async fn answer_headless_offer(
    state: Rc<RefCell<HeadlessNodeState>>,
    message: &JsValue,
) -> Result<JsValue, String> {
    let handle = headless_demo_node(&state)?;
    let offer = required_message_field(message, "offer", "paste an offer first")?;
    let answer = node::answer_offer(&handle.node.provider, offer).await?;
    ensure_headless_generation_current(&state, handle.generation)?;
    let result = Object::new();
    js_set(&result, "answer", &JsValue::from_str(&answer))?;
    js_set(&result, "message", &JsValue::from_str("answer created"))?;
    Ok(result.into())
}

async fn accept_headless_answer(
    state: Rc<RefCell<HeadlessNodeState>>,
    message: &JsValue,
) -> Result<JsValue, String> {
    let handle = headless_demo_node(&state)?;
    let answer = required_message_field(message, "answer", "paste an answer first")?;
    node::accept_answer(&handle.node.provider, answer).await?;
    headless_node_snapshot(
        state,
        "answer accepted".to_string(),
        None,
        true,
        Some(handle.generation),
    )
    .await
}

async fn route_headless_onion_proxy(
    state: Rc<RefCell<HeadlessNodeState>>,
    message: &JsValue,
) -> Result<JsValue, String> {
    let handle = headless_demo_node(&state)?;
    let request = onion::OnionProxyRouteRequest::from_js(message)?;
    let route = onion::route(&handle.node.provider, request)
        .await
        .map_err(|error| error.to_string())?;
    ensure_headless_generation_current(&state, handle.generation)?;
    route.to_js()
}

async fn request_headless_onion_proxy(
    state: Rc<RefCell<HeadlessNodeState>>,
    message: &JsValue,
) -> Result<JsValue, String> {
    let handle = headless_demo_node(&state)?;
    let request = onion::OnionProxyHttpRequest::from_js(message)?;
    let response = onion::request(&handle.node.provider, request)
        .await
        .map_err(|error| error.to_string())?;
    ensure_headless_generation_current(&state, handle.generation)?;
    response.to_js()
}

fn headless_demo_node(state: &Rc<RefCell<HeadlessNodeState>>) -> Result<HeadlessDemoNode, String> {
    let state = state.borrow();
    Ok(HeadlessDemoNode {
        node: state
            .node
            .clone()
            .ok_or_else(|| "start the node first".to_string())?,
        generation: state.generation,
    })
}

fn required_message_field(
    message: &JsValue,
    field: &'static str,
    empty_message: &'static str,
) -> Result<String, String> {
    let value = js_string_field(message, field)?.trim().to_string();
    if value.is_empty() {
        Err(empty_message.to_string())
    } else {
        Ok(value)
    }
}

fn begin_headless_start(state: &Rc<RefCell<HeadlessNodeState>>, message: String) -> u64 {
    let mut state = state.borrow_mut();
    state.generation = state.generation.wrapping_add(1);
    state.starting = true;
    state.start_error = None;
    state.message = message;
    state.peers = Vec::new();
    state.generation
}

fn set_headless_starting_for_generation(
    state: &Rc<RefCell<HeadlessNodeState>>,
    generation: u64,
    message: String,
    error: Option<String>,
    starting: bool,
) {
    let mut state = state.borrow_mut();
    if state.generation != generation {
        return;
    }
    state.starting = starting;
    state.start_error = error;
    state.message = message;
}

fn headless_generation_current(state: &Rc<RefCell<HeadlessNodeState>>, generation: u64) -> bool {
    state.borrow().generation == generation
}

fn ensure_headless_generation_current(
    state: &Rc<RefCell<HeadlessNodeState>>,
    generation: u64,
) -> Result<(), String> {
    if headless_generation_current(state, generation) {
        Ok(())
    } else {
        Err("node operation cancelled".to_string())
    }
}

pub(crate) async fn operation_timeout<T, F>(
    label: &'static str,
    timeout: Duration,
    operation: F,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    let operation = operation.fuse();
    let timer = sleep(timeout).fuse();
    futures::pin_mut!(operation, timer);
    futures::select! {
        result = operation => result,
        _ = timer => Err(format!("{label} timed out")),
    }
}

fn retained_headless_message(message: String, online: bool) -> String {
    if !message.trim().is_empty() {
        return message;
    }
    if online {
        "background node active".to_string()
    } else {
        "background node offline".to_string()
    }
}

async fn headless_node_snapshot(
    state: Rc<RefCell<HeadlessNodeState>>,
    context: String,
    required_peer: Option<PeerView>,
    settle: bool,
    expected_generation: Option<u64>,
) -> Result<JsValue, String> {
    let (node, account, state_peers, starting, start_error, state_message, generation) = {
        let state = state.borrow();
        (
            state.node.clone(),
            state.wallet_account.clone(),
            state.peers.clone(),
            state.starting,
            state.start_error.clone(),
            state.message.clone(),
            state.generation,
        )
    };
    if expected_generation.is_some_and(|expected| expected != generation) {
        let state = HeadlessSnapshotState {
            node: node.as_ref(),
            peers: &state_peers,
            account: account.as_ref(),
            starting,
            start_error: start_error.as_deref(),
            message: state_message,
        };
        return retained_headless_snapshot_js(state);
    }
    let Some(node) = node else {
        let state = HeadlessSnapshotState {
            node: None,
            peers: &[],
            account: None,
            starting,
            start_error: start_error.as_deref(),
            message: state_message,
        };
        return retained_headless_snapshot_js(state);
    };

    let mut peers = state_peers;
    let mut message = context.clone();
    let delays: &[u64] = if settle {
        peer_sync::PEER_SETTLE_DELAYS_MS
    } else {
        &[0]
    };
    for delay_ms in delays {
        if *delay_ms > 0 {
            sleep(Duration::from_millis(*delay_ms)).await;
        }
        match node::list_peers(&node.provider).await {
            Ok(next) => {
                peers = if let Some(required_peer) = required_peer.as_ref() {
                    peer_sync::merge_required_peer(next, required_peer)
                } else {
                    next
                };
                if settle {
                    message = peer_sync::peer_sync_status(&context, peers.len());
                }
            }
            Err(error) => {
                message = format!("{context}; peer sync failed: {error}");
            }
        }
    }
    {
        let mut state = state.borrow_mut();
        if state.generation != generation || state.node.is_none() {
            return retained_live_headless_snapshot_js(&state);
        }
        state.peers = peers.clone();
        if settle {
            state.message = message.clone();
        }
    }

    headless_snapshot_js(
        true,
        node.provider.address(),
        &peers,
        account.as_ref(),
        message,
        starting,
        start_error.as_deref(),
    )
}

struct HeadlessSnapshotState<'a> {
    node: Option<&'a DemoNode>,
    peers: &'a [PeerView],
    account: Option<&'a WalletAccount>,
    message: String,
    starting: bool,
    start_error: Option<&'a str>,
}

fn retained_live_headless_snapshot_js(state: &HeadlessNodeState) -> Result<JsValue, String> {
    let snapshot = HeadlessSnapshotState {
        node: state.node.as_ref(),
        peers: &state.peers,
        account: state.wallet_account.as_ref(),
        message: state.message.clone(),
        starting: state.starting,
        start_error: state.start_error.as_deref(),
    };
    retained_headless_snapshot_js(snapshot)
}

fn retained_headless_snapshot_js(state: HeadlessSnapshotState<'_>) -> Result<JsValue, String> {
    let online = state.node.is_some();
    let did = state
        .node
        .map(|node| node.provider.address())
        .unwrap_or_default();
    let peers = if online { state.peers } else { &[] };
    let account = if online { state.account } else { None };
    let message = retained_headless_message(state.message, online);
    headless_snapshot_js(
        online,
        did,
        peers,
        account,
        message,
        state.starting,
        state.start_error,
    )
}

fn headless_snapshot_js(
    online: bool,
    did: String,
    peers: &[PeerView],
    account: Option<&WalletAccount>,
    message: String,
    starting: bool,
    error: Option<&str>,
) -> Result<JsValue, String> {
    let snapshot = Object::new();
    js_set(&snapshot, "online", &JsValue::from_bool(online))?;
    js_set(&snapshot, "starting", &JsValue::from_bool(starting))?;
    js_set(&snapshot, "did", &JsValue::from_str(&did))?;
    js_set(&snapshot, "message", &JsValue::from_str(&message))?;
    js_set(&snapshot, "peers", &peer_views_js(peers))?;
    if let Some(error) = error {
        js_set(&snapshot, "error", &JsValue::from_str(error))?;
    }
    if let Some(account) = account {
        js_set(
            &snapshot,
            "walletKind",
            &JsValue::from_str(account.kind.value()),
        )?;
        js_set(&snapshot, "account", &JsValue::from_str(&account.account))?;
        js_set(
            &snapshot,
            "accountType",
            &JsValue::from_str(&account.account_type),
        )?;
    }
    Ok(snapshot.into())
}

fn peer_views_js(peers: &[PeerView]) -> JsValue {
    let array = Array::new();
    for peer in peers {
        let object = Object::new();
        let _did = js_set(&object, "did", &JsValue::from_str(peer.did()));
        let _state = js_set(&object, "state", &JsValue::from_str(peer.state()));
        array.push(&object.into());
    }
    array.into()
}

fn extension_start_settings_from_js(value: &JsValue) -> ExtensionStartSettings {
    ExtensionStartSettings {
        network_id: js_string_field(value, "networkId").unwrap_or_else(|_| "1".to_string()),
        ice_servers: js_string_field(value, "iceServers")
            .unwrap_or_else(|_| "stun://stun.l.google.com:19302".to_string()),
        stabilize_interval: js_string_field(value, "stabilizeInterval")
            .unwrap_or_else(|_| "3".to_string()),
        storage_name: js_string_field(value, "storageName")
            .unwrap_or_else(|_| "rings-frontend".to_string()),
        seed_url: js_string_field(value, "seedUrl").unwrap_or_default(),
    }
}

fn send_node_response(send_response: Function, response: Result<JsValue, String>) {
    let _sent = send_response.call1(&JsValue::NULL, &runtime_response(response));
}

fn runtime_response(response: Result<JsValue, String>) -> JsValue {
    let object = Object::new();
    match response {
        Ok(result) => {
            let _ok = js_set(&object, "ok", &JsValue::TRUE);
            let _result = js_set(&object, "result", &result);
        }
        Err(error) => {
            let _ok = js_set(&object, "ok", &JsValue::FALSE);
            let _error = js_set(&object, "error", &JsValue::from_str(&error));
        }
    }
    object.into()
}
