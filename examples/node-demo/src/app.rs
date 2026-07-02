//! Yew application for the unified node demo.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use futures::FutureExt;
use gloo_timers::callback::Interval;
use gloo_timers::future::sleep;
use js_sys::Array;
use js_sys::Function;
use js_sys::Object;
use js_sys::Promise;
use js_sys::Reflect;
use rings_node::extension::snark::ProofResult;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::Event;
use web_sys::HtmlInputElement;
use web_sys::HtmlSelectElement;
use web_sys::HtmlTextAreaElement;
use web_sys::InputEvent;
use yew::prelude::*;

use crate::custom;
use crate::dweb;
use crate::node;
use crate::node::DemoNode;
use crate::node::NodeSettings;
use crate::node::PeerView;
use crate::proof;
use crate::styles;
use crate::wallet;
use crate::wallet::WalletAccount;
use crate::wallet::WalletKind;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Panel {
    Dweb,
    Proof,
    Custom,
}

impl Panel {
    fn label(self) -> &'static str {
        match self {
            Self::Dweb => "Dweb",
            Self::Proof => "Proof",
            Self::Custom => "Custom",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SdpMode {
    Initiator,
    Responder,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LinkTab {
    ManualSdp,
    HttpEndpoint,
}

const CHROME_WEBRTC_DEBUG_URL: &str = "chrome://webrtc-internals/";
const FIREFOX_WEBRTC_DEBUG_URL: &str = "about:webrtc";
const CHROME_EXTENSION_MANAGER_URL: &str = "chrome://extensions/";
const FIREFOX_EXTENSION_MANAGER_URL: &str = "about:debugging#/runtime/this-firefox";
const EXTENSION_NODE_BRIDGE: &str = "RingsExtensionNodeBridge";
const EXTENSION_NODE_TARGET: &str = "rings.node.offscreen";
const EXTENSION_NODE_START: &str = "rings.node.start";
const EXTENSION_NODE_STOP: &str = "rings.node.stop";
const EXTENSION_NODE_STATUS: &str = "rings.node.status";
const EXTENSION_NODE_CONNECT_HTTP: &str = "rings.node.connectHttp";
const EXTENSION_NODE_CREATE_OFFER: &str = "rings.node.createOffer";
const EXTENSION_NODE_ANSWER_OFFER: &str = "rings.node.answerOffer";
const EXTENSION_NODE_ACCEPT_ANSWER: &str = "rings.node.acceptAnswer";
const SETTING_WALLET_KIND: &str = "rings.node-demo.walletKind";
const SETTING_NETWORK_ID: &str = "rings.node-demo.networkId";
const SETTING_ICE_SERVERS: &str = "rings.node-demo.iceServers";
const SETTING_STABILIZE_INTERVAL: &str = "rings.node-demo.stabilizeInterval";
const SETTING_STORAGE_NAME: &str = "rings.node-demo.storageName";
const SETTING_SEED_URL: &str = "rings.node-demo.seedUrl";
const SETTING_HTTP_ENDPOINT: &str = "rings.node-demo.httpEndpoint";
const WALLET_CONNECT_TIMEOUT: Duration = Duration::from_secs(45);
const SESSION_AUTH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Eq, PartialEq)]
enum UiIcon {
    Power,
    PowerOff,
    Terminal,
    Sliders,
    PanelOpen,
    PanelClose,
}

fn ui_icon(icon: UiIcon) -> Html {
    let content = match icon {
        UiIcon::Power => html! {
            <>
                <path d="M12 3.5v7" />
                <path d="M7.2 6.8a7 7 0 1 0 9.6 0" />
            </>
        },
        UiIcon::PowerOff => html! {
            <>
                <path d="M4.5 4.5l15 15" />
                <path d="M12 3.5v4.6" />
                <path d="M8.1 7.3a7 7 0 1 0 8.2.3" />
            </>
        },
        UiIcon::Terminal => html! {
            <>
                <rect x="4.5" y="5" width="15" height="14" rx="2.2" />
                <path d="M8 10l2.6 2L8 14" />
                <path d="M13 15h3.5" />
            </>
        },
        UiIcon::Sliders => html! {
            <>
                <path d="M5 6h14" />
                <path d="M5 12h14" />
                <path d="M5 18h14" />
                <circle cx="9" cy="6" r="1.7" />
                <circle cx="15" cy="12" r="1.7" />
                <circle cx="11.5" cy="18" r="1.7" />
            </>
        },
        UiIcon::PanelOpen => html! {
            <>
                <rect x="4.5" y="5" width="15" height="14" rx="2.2" />
                <path d="M14.5 5v14" />
                <path d="M13 9l-3 3 3 3" />
            </>
        },
        UiIcon::PanelClose => html! {
            <>
                <rect x="4.5" y="5" width="15" height="14" rx="2.2" />
                <path d="M14.5 5v14" />
                <path d="M9.5 9l3 3-3 3" />
            </>
        },
    };

    html! {
        <svg
            class="ui-icon"
            viewBox="0 0 24 24"
            aria-hidden="true"
            focusable="false"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
            stroke-linejoin="round"
        >
            { content }
        </svg>
    }
}

/// Unified Rings node demo app.
#[function_component(App)]
pub fn app() -> Html {
    let active_panel = use_state(|| Panel::Dweb);
    let wallet_kind = use_state(|| {
        load_setting(SETTING_WALLET_KIND)
            .map(|value| WalletKind::from_value(&value))
            .unwrap_or(WalletKind::WebCrypto)
    });
    let wallet_account = use_state(|| None::<WalletAccount>);
    let node_starting = use_state(|| false);
    let node_ref = use_mut_ref(|| None::<DemoNode>);
    let site = use_mut_ref(default_site);

    let did = use_state(String::new);
    let status = use_state(|| "select an account standard and start the browser node".to_string());
    let network_id =
        use_state(|| load_setting(SETTING_NETWORK_ID).unwrap_or_else(|| "1".to_string()));
    let ice_servers = use_state(|| {
        load_setting(SETTING_ICE_SERVERS)
            .unwrap_or_else(|| "stun://stun.l.google.com:19302".to_string())
    });
    let stabilize_interval =
        use_state(|| load_setting(SETTING_STABILIZE_INTERVAL).unwrap_or_else(|| "3".to_string()));
    let storage_name = use_state(|| {
        load_setting(SETTING_STORAGE_NAME).unwrap_or_else(|| "rings-node-demo".to_string())
    });
    let peers = use_state(Vec::<PeerView>::new);

    let seed_url = use_state(|| load_setting(SETTING_SEED_URL).unwrap_or_default());
    let http_endpoint = use_state(|| {
        load_setting(SETTING_HTTP_ENDPOINT).unwrap_or_else(|| "http://127.0.0.1:50001".to_string())
    });
    let sdp_remote_did = use_state(String::new);
    let generated_offer = use_state(String::new);
    let remote_offer = use_state(String::new);
    let generated_answer = use_state(String::new);
    let remote_answer = use_state(String::new);
    let sdp_mode = use_state(|| SdpMode::Initiator);
    let link_dialog_open = use_state(|| false);
    let link_tab = use_state(|| LinkTab::ManualSdp);
    let settings_dialog_open = use_state(|| false);
    let control_sidebar_collapsed = use_state(|| false);
    let workbench_dialog_open = use_state(|| false);

    let host_path = use_state(|| "/".to_string());
    let host_body = use_state(default_page);
    let hosted_pages = use_state(|| vec![("/".to_string(), default_page())]);
    let fetch_peer = use_state(String::new);
    let fetch_path = use_state(|| "/".to_string());
    let dweb_page = use_state(String::new);

    let prover_did = use_state(String::new);
    let r1cs_url = use_state(|| "http://127.0.0.1:8080/simple_bn256.r1cs".to_string());
    let wasm_url = use_state(|| "http://127.0.0.1:8080/simple_bn256.wasm".to_string());

    let custom_namespace = use_state(|| "custom".to_string());
    let custom_registered = use_state(|| vec!["custom".to_string(), "example".to_string()]);
    let custom_peer = use_state(String::new);
    let custom_payload = use_state(|| "hello from Rings".to_string());
    let custom_events = use_state(Vec::<custom::CustomEvent>::new);

    let on_wallet_kind = {
        let wallet_kind = wallet_kind.clone();
        Callback::from(move |event: Event| {
            if let Some(value) = select_value(&event) {
                wallet_kind.set(WalletKind::from_value(&value));
            }
        })
    };

    {
        let settings_snapshot = (
            (*wallet_kind).value().to_string(),
            (*network_id).clone(),
            (*ice_servers).clone(),
            (*stabilize_interval).clone(),
            (*storage_name).clone(),
            (*seed_url).clone(),
            (*http_endpoint).clone(),
        );
        use_effect_with(settings_snapshot, move |settings| {
            save_setting(SETTING_WALLET_KIND, &settings.0);
            save_setting(SETTING_NETWORK_ID, &settings.1);
            save_setting(SETTING_ICE_SERVERS, &settings.2);
            save_setting(SETTING_STABILIZE_INTERVAL, &settings.3);
            save_setting(SETTING_STORAGE_NAME, &settings.4);
            save_setting(SETTING_SEED_URL, &settings.5);
            save_setting(SETTING_HTTP_ENDPOINT, &settings.6);
        });
    }

    {
        let did = did.clone();
        let peers = peers.clone();
        let wallet_account = wallet_account.clone();
        let status = status.clone();
        use_effect_with((), move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                let Some(bridge) = extension_node_bridge() else {
                    return;
                };
                match extension_node_status(&bridge).await {
                    Ok(snapshot) if snapshot.online => {
                        did.set(snapshot.did);
                        peers.set(snapshot.peers);
                        wallet_account.set(snapshot.wallet_account);
                        status.set("background node active".to_string());
                    }
                    Ok(_) => {}
                    Err(error) => status.set(format!("background status failed: {error}")),
                }
            });
        });
    }

    let on_start = {
        let wallet_kind = wallet_kind.clone();
        let wallet_account = wallet_account.clone();
        let node_starting = node_starting.clone();
        let node_ref = node_ref.clone();
        let site = site.clone();
        let did = did.clone();
        let status = status.clone();
        let peers = peers.clone();
        let network_id = network_id.clone();
        let ice_servers = ice_servers.clone();
        let stabilize_interval = stabilize_interval.clone();
        let storage_name = storage_name.clone();
        let seed_url = seed_url.clone();
        let dweb_page = dweb_page.clone();
        let custom_events = custom_events.clone();
        let settings_dialog_open = settings_dialog_open.clone();
        Callback::from(move |_| {
            let status = status.clone();
            let peers = peers.clone();
            let wallet_account = wallet_account.clone();
            let node_starting = node_starting.clone();
            let node_ref = node_ref.clone();
            let site = site.clone();
            let did = did.clone();
            let settings_dialog_open = settings_dialog_open.clone();
            let network_id = (*network_id).clone();
            let ice_servers = (*ice_servers).clone();
            let stabilize_interval = (*stabilize_interval).clone();
            let storage_name = (*storage_name).clone();
            let seed_url = (*seed_url).trim().to_string();
            let dweb_page = dweb_page.clone();
            let custom_events = custom_events.clone();
            let kind = *wallet_kind;
            node_starting.set(true);
            status.set(format!("connecting {}", kind.label()));
            wasm_bindgen_futures::spawn_local(async move {
                if let Some(bridge) = extension_node_bridge() {
                    match extension_node_start(
                        &bridge,
                        kind,
                        ExtensionStartSettings {
                            network_id,
                            ice_servers,
                            stabilize_interval,
                            storage_name,
                            seed_url,
                        },
                    )
                    .await
                    {
                        Ok(snapshot) => {
                            *node_ref.borrow_mut() = None;
                            settings_dialog_open.set(false);
                            apply_extension_snapshot(
                                snapshot,
                                &did,
                                &peers,
                                &wallet_account,
                                &node_starting,
                                &status,
                            );
                            if let Err(error) = poll_extension_node_start(
                                &bridge,
                                did,
                                peers,
                                wallet_account,
                                node_starting.clone(),
                                status.clone(),
                            )
                            .await
                            {
                                node_starting.set(false);
                                status.set(error);
                            }
                        }
                        Err(error) => {
                            node_starting.set(false);
                            status.set(error);
                        }
                    }
                    return;
                }

                let settings = match node_settings(
                    network_id,
                    ice_servers,
                    stabilize_interval,
                    storage_name,
                ) {
                    Ok(settings) => settings,
                    Err(error) => {
                        node_starting.set(false);
                        status.set(error);
                        return;
                    }
                };
                let account = match operation_timeout(
                    "account authorization",
                    WALLET_CONNECT_TIMEOUT,
                    wallet::connect(kind),
                )
                .await
                {
                    Ok(account) => account,
                    Err(error) => {
                        node_starting.set(false);
                        status.set(error);
                        return;
                    }
                };
                status.set("authorizing session key".to_string());
                let built = match operation_timeout(
                    "session authorization",
                    SESSION_AUTH_TIMEOUT,
                    node::build_node(&account, settings),
                )
                .await
                {
                    Ok(node) => node,
                    Err(error) => {
                        node_starting.set(false);
                        status.set(error);
                        return;
                    }
                };
                let my_did = built.provider.address();
                site.borrow_mut().insert(
                    "/".to_string(),
                    format!(
                        "<h1>Rings node {my_did}</h1><p>Served by the unified browser demo.</p>"
                    ),
                );
                let on_dweb_response = {
                    let dweb_page = dweb_page.clone();
                    Callback::from(move |response: dweb::DwebResponse| {
                        dweb_page.set(format!("<!-- {} -->\n{}", response.path, response.body));
                    })
                };
                if let Err(error) = dweb::register(&built.provider, site.clone(), on_dweb_response)
                {
                    node_starting.set(false);
                    status.set(error);
                    return;
                }
                let on_custom = {
                    let custom_events = custom_events.clone();
                    Callback::from(move |event: custom::CustomEvent| {
                        let mut next = (*custom_events).clone();
                        next.insert(0, event);
                        next.truncate(20);
                        custom_events.set(next);
                    })
                };
                for namespace in ["custom", "example"] {
                    if let Err(error) =
                        custom::register(&built.provider, namespace.to_string(), on_custom.clone())
                    {
                        node_starting.set(false);
                        status.set(error);
                        return;
                    }
                }
                did.set(my_did);
                wallet_account.set(Some(account));
                *node_ref.borrow_mut() = Some(built.clone());
                settings_dialog_open.set(false);
                node_starting.set(false);
                if seed_url.is_empty() {
                    status.set("node ready".to_string());
                    return;
                }
                status.set(format!("node ready; connecting seed {seed_url}"));
                match node::connect_http(&built.provider, seed_url).await {
                    Ok(seed_did) => {
                        let seed_peer = PeerView {
                            did: seed_did,
                            state: "Connected".to_string(),
                        };
                        sync_peers_after_handshake(
                            built,
                            peers,
                            status,
                            "seed URL connected",
                            Some(seed_peer),
                        )
                        .await;
                    }
                    Err(error) => status.set(format!("node ready; seed connect failed: {error}")),
                }
            });
        })
    };

    let on_disconnect = {
        let wallet_account = wallet_account.clone();
        let node_starting = node_starting.clone();
        let node_ref = node_ref.clone();
        let did = did.clone();
        let status = status.clone();
        let peers = peers.clone();
        let generated_offer = generated_offer.clone();
        let remote_offer = remote_offer.clone();
        let generated_answer = generated_answer.clone();
        let remote_answer = remote_answer.clone();
        let link_dialog_open = link_dialog_open.clone();
        let settings_dialog_open = settings_dialog_open.clone();
        Callback::from(move |_| {
            if let Some(bridge) = extension_node_bridge() {
                did.set(String::new());
                wallet_account.set(None);
                node_starting.set(false);
                peers.set(Vec::new());
                generated_offer.set(String::new());
                remote_offer.set(String::new());
                generated_answer.set(String::new());
                remote_answer.set(String::new());
                link_dialog_open.set(false);
                settings_dialog_open.set(false);
                status.set("stopping background node".to_string());
                let status = status.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match extension_node_stop(&bridge).await {
                        Ok(message) => status.set(message),
                        Err(error) => status.set(format!("background stop failed: {error}")),
                    }
                });
                return;
            }

            let Some(node) = node_ref.borrow_mut().take() else {
                status.set("node already offline".to_string());
                return;
            };
            let provider = node.provider.clone();
            did.set(String::new());
            wallet_account.set(None);
            node_starting.set(false);
            peers.set(Vec::new());
            generated_offer.set(String::new());
            remote_offer.set(String::new());
            generated_answer.set(String::new());
            remote_answer.set(String::new());
            link_dialog_open.set(false);
            settings_dialog_open.set(false);
            status.set("node disconnected".to_string());

            let status = status.clone();
            let did = did.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let cleanup = node::disconnect_all(&provider).fuse();
                let timeout = sleep(Duration::from_secs(2)).fuse();
                futures::pin_mut!(cleanup, timeout);
                let message = futures::select! {
                    result = cleanup => match result {
                        Ok(count) if count == 0 => "node disconnected".to_string(),
                        Ok(count) => format!("node disconnected; closed {count} peer links"),
                        Err(error) => format!("node disconnected; peer cleanup failed: {error}"),
                    },
                    _ = timeout => "node disconnected; peer cleanup timed out".to_string(),
                };
                node.stop();
                if (*did).is_empty() {
                    status.set(message);
                }
            });
        })
    };

    {
        let node_ref = node_ref.clone();
        let peers = peers.clone();
        let did = did.clone();
        let wallet_account = wallet_account.clone();
        let node_starting = node_starting.clone();
        let node_online = !(*did).is_empty();
        use_effect_with(node_online, move |online| {
            let interval = if *online {
                Some(Interval::new(4_000, move || {
                    if let Some(bridge) = extension_node_bridge() {
                        let did = did.clone();
                        let peers = peers.clone();
                        let wallet_account = wallet_account.clone();
                        let node_starting = node_starting.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            if let Ok(snapshot) = extension_node_status(&bridge).await {
                                if snapshot.online {
                                    did.set(snapshot.did);
                                    peers.set(snapshot.peers);
                                    wallet_account.set(snapshot.wallet_account);
                                }
                                node_starting.set(snapshot.starting);
                            }
                        });
                        return;
                    }
                    let Some(node) = node_ref.borrow().clone() else {
                        return;
                    };
                    let peers = peers.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Ok(next) = node::list_peers(&node.provider).await {
                            peers.set(next);
                        }
                    });
                }))
            } else {
                None
            };
            move || drop(interval)
        });
    }

    let control_view = ControlView {
        wallet_kind: *wallet_kind,
        wallet_account: (*wallet_account).clone(),
        node_starting: *node_starting,
        did: &did,
        status: &status,
        peers: &peers,
        network_id: &network_id,
        ice_servers: &ice_servers,
        stabilize_interval: &stabilize_interval,
        storage_name: &storage_name,
        seed_url: &seed_url,
    };
    let launch_actions = LaunchActions {
        on_wallet_kind,
        on_start,
        on_disconnect,
    };
    let session_view = SessionView {
        wallet_account: (*wallet_account).clone(),
        did: &did,
        peers: &peers,
    };
    let link_control = link_control(
        ConnectState {
            http_endpoint: &http_endpoint,
            sdp_remote_did: &sdp_remote_did,
            generated_offer: &generated_offer,
            remote_offer: &remote_offer,
            generated_answer: &generated_answer,
            remote_answer: &remote_answer,
            sdp_mode: &sdp_mode,
            link_dialog_open: &link_dialog_open,
            link_tab: &link_tab,
            launcher_hidden: *settings_dialog_open || *workbench_dialog_open,
        },
        node_ref.clone(),
        peers.clone(),
        status.clone(),
    );
    let workbench_body = match *active_panel {
        Panel::Dweb => html! {
            { dweb_panel(
                DwebState {
                    host_path: &host_path,
                    host_body: &host_body,
                    hosted_pages: &hosted_pages,
                    fetch_peer: &fetch_peer,
                    fetch_path: &fetch_path,
                    dweb_page: &dweb_page,
                },
                site.clone(),
                node_ref.clone(),
                status.clone(),
            ) }
        },
        Panel::Proof => html! {
            { proof_panel(
                &prover_did,
                &r1cs_url,
                &wasm_url,
                node_ref.clone(),
                status.clone(),
            ) }
        },
        Panel::Custom => html! {
            { custom_panel(
                &custom_namespace,
                &custom_registered,
                &custom_peer,
                &custom_payload,
                &custom_events,
                node_ref.clone(),
                status.clone(),
            ) }
        },
    };
    let workbench_control = workbench_control(
        *active_panel,
        active_panel.clone(),
        workbench_dialog_open.clone(),
        workbench_body,
    );
    let control_sidebar = control_sidebar(
        control_view,
        launch_actions,
        workbench_control,
        settings_dialog_open.clone(),
        control_sidebar_collapsed.clone(),
    );

    html! {
        <main class="app-shell topology-shell">
            <style>{ styles::APP_CSS }</style>
            { app_header() }
            { network_stage(session_view, &status, link_control, control_sidebar) }
        </main>
    }
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
        starting: false,
        start_error: None,
        message: "background node offline".to_string(),
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

fn default_site() -> HashMap<String, String> {
    HashMap::from([("/".to_string(), default_page())])
}

fn default_page() -> String {
    "<h1>Hello from Rings</h1><p>This page is hosted by a browser node.</p>".to_string()
}

fn node_settings(
    network_id: String,
    ice_servers: String,
    stabilize_interval: String,
    storage_name: String,
) -> Result<NodeSettings, String> {
    let network_id = network_id
        .trim()
        .parse::<u32>()
        .map_err(|error| format!("invalid network id: {error}"))?;
    let stabilize_interval = stabilize_interval
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid stabilize interval: {error}"))?;
    Ok(NodeSettings {
        network_id,
        ice_servers,
        stabilize_interval,
        storage_name,
    })
}

async fn handle_headless_node_message(
    state: Rc<RefCell<HeadlessNodeState>>,
    message_type: String,
    message: JsValue,
) -> Result<JsValue, String> {
    match message_type.as_str() {
        EXTENSION_NODE_STATUS => {
            headless_node_snapshot(state, "background node active".to_string(), None, false).await
        }
        EXTENSION_NODE_START => start_headless_node(state, &message).await,
        EXTENSION_NODE_STOP => stop_headless_node(state).await,
        EXTENSION_NODE_CONNECT_HTTP => connect_headless_node_http(state, &message).await,
        EXTENSION_NODE_CREATE_OFFER => create_headless_offer(state, &message).await,
        EXTENSION_NODE_ANSWER_OFFER => answer_headless_offer(state, &message).await,
        EXTENSION_NODE_ACCEPT_ANSWER => accept_headless_answer(state, &message).await,
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
        )
        .await;
    }
    if state.borrow().starting {
        return headless_node_snapshot(state, "background node starting".to_string(), None, false)
            .await;
    }

    let settings_value = js_prop(message, "settings")?;
    let wallet_kind = js_string_field(&settings_value, "walletKind")
        .map(|value| WalletKind::from_value(&value))
        .unwrap_or(WalletKind::WebCrypto);
    let settings = extension_start_settings_from_js(&settings_value);
    set_headless_starting(
        &state,
        format!("connecting {}", wallet_kind.label()),
        None,
        true,
    );
    wasm_bindgen_futures::spawn_local(run_headless_node_start(
        state.clone(),
        wallet_kind,
        settings,
    ));
    headless_node_snapshot(state, "background node starting".to_string(), None, false).await
}

async fn run_headless_node_start(
    state: Rc<RefCell<HeadlessNodeState>>,
    wallet_kind: WalletKind,
    settings: ExtensionStartSettings,
) {
    match start_headless_node_inner(state.clone(), wallet_kind, settings).await {
        Ok(message) => set_headless_starting(&state, message, None, false),
        Err(error) => set_headless_starting(&state, error.clone(), Some(error), false),
    }
}

async fn start_headless_node_inner(
    state: Rc<RefCell<HeadlessNodeState>>,
    wallet_kind: WalletKind,
    settings: ExtensionStartSettings,
) -> Result<String, String> {
    let node_settings = node_settings(
        settings.network_id,
        settings.ice_servers,
        settings.stabilize_interval,
        settings.storage_name,
    )?;
    let account = operation_timeout(
        "account authorization",
        WALLET_CONNECT_TIMEOUT,
        wallet::connect(wallet_kind),
    )
    .await?;
    set_headless_starting(&state, "authorizing session key".to_string(), None, true);
    let built = operation_timeout(
        "session authorization",
        SESSION_AUTH_TIMEOUT,
        node::build_node(&account, node_settings),
    )
    .await?;
    set_headless_starting(&state, "registering node protocols".to_string(), None, true);
    let my_did = built.provider.address();
    let site = Rc::new(RefCell::new(default_site()));
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
    for namespace in ["custom", "example"] {
        custom::register(&built.provider, namespace.to_string(), on_custom.clone())?;
    }

    {
        let mut state = state.borrow_mut();
        state.wallet_account = Some(account);
        state.node = Some(built.clone());
    }

    let seed_url = settings.seed_url.trim().to_string();
    if seed_url.is_empty() {
        return Ok("background node ready".to_string());
    }

    set_headless_starting(
        &state,
        format!("background node ready; connecting seed {seed_url}"),
        None,
        true,
    );
    match node::connect_http(&built.provider, seed_url).await {
        Ok(seed_did) => {
            let seed_peer = PeerView {
                did: seed_did,
                state: "Connected".to_string(),
            };
            let _snapshot = headless_node_snapshot(
                state.clone(),
                "seed URL connected".to_string(),
                Some(seed_peer),
                true,
            )
            .await;
            Ok("seed URL connected".to_string())
        }
        Err(error) => Ok(format!(
            "background node ready; seed connect failed: {error}"
        )),
    }
}

async fn stop_headless_node(state: Rc<RefCell<HeadlessNodeState>>) -> Result<JsValue, String> {
    let node = {
        let mut state = state.borrow_mut();
        state.wallet_account = None;
        state.starting = false;
        state.start_error = None;
        state.message = "background node stopped".to_string();
        state.node.take()
    };
    let Some(node) = node else {
        return Ok(headless_snapshot_js(
            false,
            String::new(),
            &[],
            None,
            "background node already offline".to_string(),
            false,
            None,
        )?);
    };

    let provider = node.provider.clone();
    let cleanup = node::disconnect_all(&provider).fuse();
    let timeout = sleep(Duration::from_secs(2)).fuse();
    futures::pin_mut!(cleanup, timeout);
    let message = futures::select! {
        result = cleanup => match result {
            Ok(count) if count == 0 => "background node stopped".to_string(),
            Ok(count) => format!("background node stopped; closed {count} peer links"),
            Err(error) => format!("background node stopped; peer cleanup failed: {error}"),
        },
        _ = timeout => "background node stopped; peer cleanup timed out".to_string(),
    };
    node.stop();
    Ok(headless_snapshot_js(
        false,
        String::new(),
        &[],
        None,
        message,
        false,
        None,
    )?)
}

async fn connect_headless_node_http(
    state: Rc<RefCell<HeadlessNodeState>>,
    message: &JsValue,
) -> Result<JsValue, String> {
    let node = headless_demo_node(&state)?;
    let endpoint = js_string_field(message, "endpoint")?.trim().to_string();
    if endpoint.is_empty() {
        return Err("enter a seed HTTP endpoint".to_string());
    }
    let seed_did = node::connect_http(&node.provider, endpoint).await?;
    let seed_peer = PeerView {
        did: seed_did,
        state: "Connected".to_string(),
    };
    headless_node_snapshot(
        state,
        "HTTP endpoint connected".to_string(),
        Some(seed_peer),
        true,
    )
    .await
}

async fn create_headless_offer(
    state: Rc<RefCell<HeadlessNodeState>>,
    message: &JsValue,
) -> Result<JsValue, String> {
    let node = headless_demo_node(&state)?;
    let remote_did = js_string_field(message, "did")?.trim().to_string();
    if remote_did.is_empty() {
        return Err("enter a remote DID".to_string());
    }
    let offer = node::create_offer(&node.provider, remote_did).await?;
    let result = Object::new();
    js_set(&result, "offer", &JsValue::from_str(&offer))?;
    js_set(&result, "message", &JsValue::from_str("offer created"))?;
    Ok(result.into())
}

async fn answer_headless_offer(
    state: Rc<RefCell<HeadlessNodeState>>,
    message: &JsValue,
) -> Result<JsValue, String> {
    let node = headless_demo_node(&state)?;
    let offer = js_string_field(message, "offer")?.trim().to_string();
    if offer.is_empty() {
        return Err("paste an offer first".to_string());
    }
    let answer = node::answer_offer(&node.provider, offer).await?;
    let result = Object::new();
    js_set(&result, "answer", &JsValue::from_str(&answer))?;
    js_set(&result, "message", &JsValue::from_str("answer created"))?;
    Ok(result.into())
}

async fn accept_headless_answer(
    state: Rc<RefCell<HeadlessNodeState>>,
    message: &JsValue,
) -> Result<JsValue, String> {
    let node = headless_demo_node(&state)?;
    let answer = js_string_field(message, "answer")?.trim().to_string();
    if answer.is_empty() {
        return Err("paste an answer first".to_string());
    }
    node::accept_answer(&node.provider, answer).await?;
    headless_node_snapshot(state, "answer accepted".to_string(), None, true).await
}

fn headless_demo_node(state: &Rc<RefCell<HeadlessNodeState>>) -> Result<DemoNode, String> {
    state
        .borrow()
        .node
        .clone()
        .ok_or_else(|| "start the node first".to_string())
}

fn set_headless_starting(
    state: &Rc<RefCell<HeadlessNodeState>>,
    message: String,
    error: Option<String>,
    starting: bool,
) {
    let mut state = state.borrow_mut();
    state.starting = starting;
    state.start_error = error;
    state.message = message;
}

async fn operation_timeout<T, F>(
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

async fn headless_node_snapshot(
    state: Rc<RefCell<HeadlessNodeState>>,
    context: String,
    required_peer: Option<PeerView>,
    settle: bool,
) -> Result<JsValue, String> {
    let (node, account, starting, start_error, state_message) = {
        let state = state.borrow();
        (
            state.node.clone(),
            state.wallet_account.clone(),
            state.starting,
            state.start_error.clone(),
            state.message.clone(),
        )
    };
    let Some(node) = node else {
        let message = if state_message.trim().is_empty() {
            "background node offline".to_string()
        } else {
            state_message
        };
        return Ok(headless_snapshot_js(
            false,
            String::new(),
            &[],
            None,
            message,
            starting,
            start_error.as_deref(),
        )?);
    };

    let mut peers = Vec::new();
    let mut message = context.clone();
    let delays: &[u64] = if settle {
        &[0, 1_000, 2_000, 4_000]
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
                    merge_required_peer(next, required_peer)
                } else {
                    next
                };
                if settle {
                    message = peer_sync_status(&context, peers.len());
                }
            }
            Err(error) => {
                message = format!("{context}; peer sync failed: {error}");
            }
        }
    }

    Ok(headless_snapshot_js(
        true,
        node.provider.address(),
        &peers,
        account.as_ref(),
        message,
        starting,
        start_error.as_deref(),
    )?)
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
        let _did = js_set(&object, "did", &JsValue::from_str(&peer.did));
        let _state = js_set(&object, "state", &JsValue::from_str(&peer.state));
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
            .unwrap_or_else(|_| "rings-node-demo".to_string()),
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

#[derive(Clone)]
struct LaunchActions {
    on_wallet_kind: Callback<Event>,
    on_start: Callback<MouseEvent>,
    on_disconnect: Callback<MouseEvent>,
}

struct ControlView<'a> {
    wallet_kind: WalletKind,
    wallet_account: Option<WalletAccount>,
    node_starting: bool,
    did: &'a UseStateHandle<String>,
    status: &'a UseStateHandle<String>,
    peers: &'a UseStateHandle<Vec<PeerView>>,
    network_id: &'a UseStateHandle<String>,
    ice_servers: &'a UseStateHandle<String>,
    stabilize_interval: &'a UseStateHandle<String>,
    storage_name: &'a UseStateHandle<String>,
    seed_url: &'a UseStateHandle<String>,
}

struct SessionView<'a> {
    wallet_account: Option<WalletAccount>,
    did: &'a UseStateHandle<String>,
    peers: &'a UseStateHandle<Vec<PeerView>>,
}

struct ConnectState<'a> {
    http_endpoint: &'a UseStateHandle<String>,
    sdp_remote_did: &'a UseStateHandle<String>,
    generated_offer: &'a UseStateHandle<String>,
    remote_offer: &'a UseStateHandle<String>,
    generated_answer: &'a UseStateHandle<String>,
    remote_answer: &'a UseStateHandle<String>,
    sdp_mode: &'a UseStateHandle<SdpMode>,
    link_dialog_open: &'a UseStateHandle<bool>,
    link_tab: &'a UseStateHandle<LinkTab>,
    launcher_hidden: bool,
}

struct ExtensionStartSettings {
    network_id: String,
    ice_servers: String,
    stabilize_interval: String,
    storage_name: String,
    seed_url: String,
}

struct ExtensionNodeSnapshot {
    online: bool,
    starting: bool,
    did: String,
    peers: Vec<PeerView>,
    wallet_account: Option<WalletAccount>,
    message: String,
    error: Option<String>,
}

struct HeadlessNodeState {
    node: Option<DemoNode>,
    wallet_account: Option<WalletAccount>,
    starting: bool,
    start_error: Option<String>,
    message: String,
}

struct DwebState<'a> {
    host_path: &'a UseStateHandle<String>,
    host_body: &'a UseStateHandle<String>,
    hosted_pages: &'a UseStateHandle<Vec<(String, String)>>,
    fetch_peer: &'a UseStateHandle<String>,
    fetch_path: &'a UseStateHandle<String>,
    dweb_page: &'a UseStateHandle<String>,
}

fn app_header() -> Html {
    html! {
        <header class="app-header">
            <div>
                <p class="eyebrow">{ "Browser node console" }</p>
                <h1>{ "Rings" }</h1>
            </div>
        </header>
    }
}

fn control_sidebar(
    view: ControlView<'_>,
    actions: LaunchActions,
    workbench_control: Html,
    settings_dialog_open: UseStateHandle<bool>,
    collapsed: UseStateHandle<bool>,
) -> Html {
    let did_value = if (**view.did).is_empty() {
        "not started".to_string()
    } else {
        (**view.did).clone()
    };
    let can_copy_did = !(**view.did).is_empty();
    let node_control_active = can_copy_did || view.node_starting;
    let node_state = if can_copy_did {
        "ready"
    } else if view.node_starting {
        "starting"
    } else {
        "offline"
    };
    let node_state_class = if can_copy_did {
        "rail-state ready"
    } else if view.node_starting {
        "rail-state starting"
    } else {
        "rail-state"
    };
    let account_standard = view
        .wallet_account
        .as_ref()
        .map(|account| account.kind.label().to_string())
        .unwrap_or_else(|| "none".to_string());
    let session_label = view
        .wallet_account
        .as_ref()
        .map(|account| account.account_type.clone())
        .unwrap_or_else(|| "not authorized".to_string());
    let peer_summary = match view.peers.len() {
        0 => "0 connected".to_string(),
        1 => "1 connected".to_string(),
        count => format!("{count} connected"),
    };
    let transport_state = if view.peers.is_empty() {
        "standby".to_string()
    } else {
        "linked".to_string()
    };
    let rail_did = if can_copy_did {
        short_did((**view.did).as_str())
    } else {
        "not started".to_string()
    };
    let last_signal = (**view.status).clone();
    let on_copy_did = copy_local_did_callback(view.did, view.status);
    let node_action_label = if node_control_active { "Stop" } else { "Start" };
    let node_action_icon = if node_control_active {
        UiIcon::PowerOff
    } else {
        UiIcon::Power
    };
    let node_action = if node_control_active {
        actions.on_disconnect.clone()
    } else {
        actions.on_start.clone()
    };
    let node_action_class = if node_control_active {
        "secondary action-button command-button stop-button"
    } else {
        "link-open command-button start-button"
    };
    let open_settings_dialog = {
        let settings_dialog_open = settings_dialog_open.clone();
        Callback::from(move |_| settings_dialog_open.set(true))
    };
    let close_settings_dialog = {
        let settings_dialog_open = settings_dialog_open.clone();
        Callback::from(move |_| settings_dialog_open.set(false))
    };
    let toggle_sidebar = {
        let collapsed = collapsed.clone();
        Callback::from(move |_| collapsed.set(!*collapsed))
    };
    let sidebar_class = if *collapsed {
        "control-sidebar collapsed"
    } else {
        "control-sidebar"
    };
    html! {
        <aside class={sidebar_class} aria-label="Node controls">
            <button
                class="sidebar-toggle"
                type="button"
                aria-label={if *collapsed { "Open controls" } else { "Collapse controls" }}
                aria-expanded={(!*collapsed).to_string()}
                aria-controls="node-control-sidebar-content"
                title={if *collapsed { "Open controls" } else { "Collapse controls" }}
                onclick={toggle_sidebar}
            >
                <span class="sidebar-toggle-icon" aria-hidden="true">
                    { ui_icon(if *collapsed { UiIcon::PanelOpen } else { UiIcon::PanelClose }) }
                </span>
                <span class="sidebar-toggle-label">
                    { if *collapsed { "Setup" } else { "Hide" } }
                </span>
            </button>
            if !*collapsed {
                <div id="node-control-sidebar-content" class="sidebar-content sidebar-command-panel">
                    <div class="command-panel-header">
                        <div>
                            <p class="eyebrow">{ "Control" }</p>
                            <h3>{ "Command deck" }</h3>
                        </div>
                        <span>{ "03" }</span>
                    </div>
                    <div class="command-grid">
                        <button
                            class={node_action_class}
                            type="button"
                            aria-label={node_action_label}
                            title={node_action_label}
                            onclick={node_action}
                        >
                            <span class="label-desktop">{ node_action_label }</span>
                            <span class="label-mobile command-icon" aria-hidden="true">
                                { ui_icon(node_action_icon) }
                                <span class="command-caption">{ node_action_label }</span>
                            </span>
                        </button>
                        { workbench_control }
                        <button class="secondary action-button command-button settings-button" type="button" aria-label="Settings" title="Settings" onclick={open_settings_dialog}>
                            <span class="label-desktop">{ "Settings" }</span>
                            <span class="label-mobile command-icon" aria-hidden="true">
                                { ui_icon(UiIcon::Sliders) }
                                <span class="command-caption">{ "Settings" }</span>
                            </span>
                        </button>
                    </div>
                    <div class="rail-telemetry" aria-label="Node telemetry">
                        <section class="rail-card">
                            <div class="rail-card-header">
                                <span>{ "Node" }</span>
                                <strong class={node_state_class}>{ node_state }</strong>
                            </div>
                            { rail_row("Standard", account_standard) }
                            { rail_row("Session", session_label) }
                        </section>
                        <section class="rail-card">
                            <div class="rail-card-header">
                                <span>{ "Identity" }</span>
                                <button
                                    class="copy-button rail-copy"
                                    type="button"
                                    disabled={!can_copy_did}
                                    onclick={on_copy_did.clone()}
                                >
                                    { "Copy" }
                                </button>
                            </div>
                            <code class="rail-did" title={did_value.clone()}>{ rail_did }</code>
                        </section>
                        <section class="rail-card">
                            <div class="rail-card-header">
                                <span>{ "Transport" }</span>
                                <strong class="rail-state">{ transport_state }</strong>
                            </div>
                            { rail_row("Exchange", "SDP / HTTP".to_string()) }
                            { rail_row("Peers", peer_summary) }
                        </section>
                        <section class="rail-card signal-card">
                            <div class="rail-card-header">
                                <span>{ "Last signal" }</span>
                            </div>
                            <p>{ last_signal }</p>
                        </section>
                    </div>
                </div>
            }
            {
                if *settings_dialog_open {
                    settings_dialog(
                        view.wallet_kind,
                        actions,
                        view.network_id,
                        view.ice_servers,
                        view.stabilize_interval,
                        view.storage_name,
                        view.seed_url,
                        view.status,
                        did_value,
                        on_copy_did,
                        can_copy_did,
                        view.wallet_account,
                        close_settings_dialog,
                    )
                } else {
                    html! {}
                }
            }
        </aside>
    }
}

fn settings_dialog(
    wallet_kind: WalletKind,
    actions: LaunchActions,
    network_id: &UseStateHandle<String>,
    ice_servers: &UseStateHandle<String>,
    stabilize_interval: &UseStateHandle<String>,
    storage_name: &UseStateHandle<String>,
    seed_url: &UseStateHandle<String>,
    status: &UseStateHandle<String>,
    did_value: String,
    on_copy_did: Callback<MouseEvent>,
    can_copy_did: bool,
    wallet_account: Option<WalletAccount>,
    close_dialog: Callback<MouseEvent>,
) -> Html {
    html! {
        <div class="modal-shell">
            <button class="dialog-backdrop" aria-label="Close settings" onclick={close_dialog.clone()}></button>
            <section class="link-dialog setup-dialog" role="dialog" aria-modal="true" aria-labelledby="settings-dialog-title">
                <header class="dialog-header">
                    <div>
                        <p class="eyebrow">{ "Node settings" }</p>
                        <h2 id="settings-dialog-title">{ "Settings" }</h2>
                    </div>
                    <button class="secondary dialog-close" onclick={close_dialog}>{ "Close" }</button>
                </header>
                <div class="dialog-body">
                    <div class="dialog-pane setup-pane">
                        <div class="setup-grid">
                            <section class="node-control-group setup-launch-section">
                                <div class="tool-header compact">
                                    <div>
                                        <p class="eyebrow">{ "Account" }</p>
                                        <h3>{ "Standard" }</h3>
                                    </div>
                                </div>
                                { wallet_provider_control(
                                    wallet_kind,
                                    actions.on_wallet_kind.clone(),
                                ) }
                            </section>
                            <section class="node-control-group setup-runtime-section">
                                <div class="tool-header compact">
                                    <div>
                                        <p class="eyebrow">{ "Runtime" }</p>
                                        <h3>{ "Settings" }</h3>
                                    </div>
                                </div>
                                { settings_controls(network_id, ice_servers, stabilize_interval, storage_name, seed_url, status) }
                            </section>
                            <section class="node-control-group setup-identity">
                                <div class="tool-header compact">
                                    <div>
                                        <p class="eyebrow">{ "Identity" }</p>
                                        <h3>{ "Local node" }</h3>
                                    </div>
                                </div>
                                { copyable_identity_value("DID", did_value, on_copy_did, can_copy_did) }
                                { account_details(wallet_account) }
                            </section>
                        </div>
                    </div>
                </div>
            </section>
        </div>
    }
}

fn workbench_control(
    active: Panel,
    active_panel: UseStateHandle<Panel>,
    dialog_open: UseStateHandle<bool>,
    body: Html,
) -> Html {
    let open_dialog = {
        let dialog_open = dialog_open.clone();
        Callback::from(move |_| dialog_open.set(true))
    };
    let close_dialog = {
        let dialog_open = dialog_open.clone();
        Callback::from(move |_| dialog_open.set(false))
    };
    html! {
        <div class="workbench-control">
            <button class="secondary action-button command-button workbench-button" type="button" aria-label="WorkBench" title="WorkBench" onclick={open_dialog}>
                <span class="label-desktop">{ "WorkBench" }</span>
                <span class="label-mobile command-icon" aria-hidden="true">
                    { ui_icon(UiIcon::Terminal) }
                    <span class="command-caption">{ "WorkBench" }</span>
                </span>
            </button>
            {
                if *dialog_open {
                    html! {
                        <div class="modal-shell">
                            <button class="dialog-backdrop" aria-label="Close workbench" onclick={close_dialog.clone()}></button>
                            <section class="link-dialog workbench-dialog" role="dialog" aria-modal="true" aria-labelledby="workbench-dialog-title">
                                <header class="dialog-header">
                                    <div>
                                        <p class="eyebrow">{ "Node workbench" }</p>
                                        <h2 id="workbench-dialog-title">{ active.label() }</h2>
                                    </div>
                                    <button class="secondary dialog-close" onclick={close_dialog}>{ "Close" }</button>
                                </header>
                                { workspace_tabs(active, active_panel) }
                                <div class="dialog-body workbench-dialog-body">
                                    { body }
                                </div>
                            </section>
                        </div>
                    }
                } else {
                    html! {}
                }
            }
        </div>
    }
}

fn network_stage(
    view: SessionView<'_>,
    status: &UseStateHandle<String>,
    link_control: Html,
    control_sidebar: Html,
) -> Html {
    let account_label = view
        .wallet_account
        .as_ref()
        .map(|account| account.account_type.as_str())
        .unwrap_or("not authorized");
    let account_standard = view
        .wallet_account
        .as_ref()
        .map(|account| account.kind.label().to_string())
        .unwrap_or_else(|| "none".to_string());
    let did_label = if (**view.did).is_empty() {
        "not started".to_string()
    } else {
        (**view.did).clone()
    };
    let node_label = if (**view.did).is_empty() {
        "offline".to_string()
    } else {
        "ready".to_string()
    };
    let transport_label = if view.peers.is_empty() {
        "standby".to_string()
    } else {
        "linked".to_string()
    };
    let can_copy_did = !(**view.did).is_empty();
    let on_copy_did = copy_local_did_callback(view.did, status);
    html! {
        <section class="network-stage topology-stage" aria-label="Network topology console">
            <div class="topology-hud">
                <div class="section-heading compact">
                    <p class="eyebrow">{ "Network / inferred" }</p>
                </div>
                <div class="session-strip" aria-label="Session summary">
                    { local_did_metric(did_label, on_copy_did, can_copy_did) }
                    { metric("Session", account_label.to_string()) }
                    { metric("Peers", view.peers.len().to_string()) }
                </div>
                <div class="mobile-telemetry-strip" aria-label="Mobile topology telemetry">
                    { metric("Node", node_label) }
                    { metric("Standard", account_standard) }
                    { metric("Transport", transport_label) }
                    { metric("Exchange", "SDP / HTTP".to_string()) }
                </div>
            </div>
            <div class="topology-layout">
                <div class="topology-wrap">
                    { link_control }
                    { topology((**view.did).as_str(), view.peers) }
                </div>
                { control_sidebar }
            </div>
            <div class="node-status-line" aria-label="Node status">
                <span>{ "Status" }</span>
                <p class="status">{ (**status).clone() }</p>
            </div>
        </section>
    }
}

fn workspace_tabs(active: Panel, active_panel: UseStateHandle<Panel>) -> Html {
    html! {
        <nav class="workspace-tabs" aria-label="Node workspace">
            { for [Panel::Dweb, Panel::Proof, Panel::Custom].into_iter().map(|panel| {
                let active_panel = active_panel.clone();
                let class = if panel == active { "workspace-tab active" } else { "workspace-tab" };
                html! {
                    <button class={class} onclick={Callback::from(move |_| active_panel.set(panel))}>
                        { panel.label() }
                    </button>
                }
            })}
        </nav>
    }
}

fn metric(label: &'static str, value: String) -> Html {
    let class = if label == "Local DID" {
        "metric local-did-metric"
    } else {
        "metric"
    };
    html! {
        <div class={class}>
            <span>{ label }</span>
            <strong>{ value }</strong>
        </div>
    }
}

fn local_did_metric(value: String, on_copy: Callback<MouseEvent>, enabled: bool) -> Html {
    html! {
        <div class="metric local-did-metric copyable-metric">
            <div class="metric-label-row">
                <span>{ "Local DID" }</span>
                <button
                    class="copy-button metric-copy"
                    type="button"
                    aria-label="Copy local DID"
                    title="Copy local DID"
                    disabled={!enabled}
                    onclick={on_copy}
                >
                    { "Copy" }
                </button>
            </div>
            <strong title={value.clone()}>{ value }</strong>
        </div>
    }
}

fn copy_local_did_callback(
    did: &UseStateHandle<String>,
    status: &UseStateHandle<String>,
) -> Callback<MouseEvent> {
    let did = (**did).clone();
    let status = (*status).clone();
    Callback::from(move |_| {
        if did.trim().is_empty() {
            status.set("start the node first".to_string());
            return;
        }
        let did = did.clone();
        let status = status.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match copy_text_to_clipboard(did).await {
                Ok(()) => status.set("local DID copied".to_string()),
                Err(error) => status.set(format!("copy DID failed: {error}")),
            }
        });
    })
}

fn rail_row(label: &'static str, value: String) -> Html {
    html! {
        <div class="rail-row">
            <span>{ label }</span>
            <strong>{ value }</strong>
        </div>
    }
}

fn identity_value(label: &'static str, value: String) -> Html {
    html! {
        <div class="identity-value">
            <span>{ label }</span>
            <code>{ value }</code>
        </div>
    }
}

fn copyable_identity_value(
    label: &'static str,
    value: String,
    on_copy: Callback<MouseEvent>,
    enabled: bool,
) -> Html {
    html! {
        <div class="identity-value copyable-identity">
            <span>{ label }</span>
            <code title={value.clone()}>{ value }</code>
            <button
                class="copy-button"
                type="button"
                aria-label="Copy local DID"
                title="Copy local DID"
                disabled={!enabled}
                onclick={on_copy}
            >
                { "Copy" }
            </button>
        </div>
    }
}

fn account_details(account: Option<WalletAccount>) -> Html {
    match account {
        Some(account) => html! {
            <div class="account-details">
                { identity_value("Standard", account.kind.label().to_string()) }
                { identity_value("Account type", account.account_type) }
                { identity_value("Account", account.account) }
            </div>
        },
        None => html! {},
    }
}

fn wallet_provider_control(wallet_kind: WalletKind, on_wallet_kind: Callback<Event>) -> Html {
    html! {
        <label class="field">
            <span>{ "Account standard" }</span>
            <select onchange={on_wallet_kind} value={wallet_kind.value()}>
                <option value="webcrypto" selected={wallet_kind == WalletKind::WebCrypto}>{ "WebCrypto P-256" }</option>
                <option value="eip191" selected={wallet_kind == WalletKind::EthereumEip191}>{ "Ethereum EIP-191" }</option>
                <option value="ed25519" selected={wallet_kind == WalletKind::SolanaEd25519}>{ "Solana Ed25519" }</option>
            </select>
        </label>
    }
}

fn settings_controls(
    network_id: &UseStateHandle<String>,
    ice_servers: &UseStateHandle<String>,
    stabilize_interval: &UseStateHandle<String>,
    storage_name: &UseStateHandle<String>,
    seed_url: &UseStateHandle<String>,
    status: &UseStateHandle<String>,
) -> Html {
    html! {
        <>
            { text_input("Seed URL", seed_url.clone()) }
            { text_input("Network ID", network_id.clone()) }
            { text_input("ICE servers", ice_servers.clone()) }
            { text_input("Stabilize interval seconds", stabilize_interval.clone()) }
            { text_input("IndexedDB storage", storage_name.clone()) }
            { webrtc_debug_controls(status) }
        </>
    }
}

fn webrtc_debug_controls(status: &UseStateHandle<String>) -> Html {
    let open_auto_webrtc = open_detected_debug_callback(DebugTarget::WebRtc, status.clone());
    let open_auto_extensions =
        open_detected_debug_callback(DebugTarget::ExtensionManager, status.clone());
    html! {
        <div class="debug-actions" aria-label="Debug shortcuts">
            <span>{ "Debug" }</span>
            <div class="debug-action-row">
                <button class="secondary" type="button" onclick={open_auto_webrtc}>{ "WebRTC dashboard" }</button>
                <button class="secondary" type="button" onclick={open_auto_extensions}>{ "Extension manager" }</button>
            </div>
        </div>
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BrowserKind {
    Chrome,
    Firefox,
    Unknown,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DebugTarget {
    WebRtc,
    ExtensionManager,
}

impl BrowserKind {
    fn label(self) -> &'static str {
        match self {
            Self::Chrome => "Chrome",
            Self::Firefox => "Firefox",
            Self::Unknown => "current browser",
        }
    }
}

impl DebugTarget {
    fn label(self) -> &'static str {
        match self {
            Self::WebRtc => "WebRTC debug",
            Self::ExtensionManager => "extension manager",
        }
    }
}

fn open_detected_debug_callback(
    target: DebugTarget,
    status: UseStateHandle<String>,
) -> Callback<MouseEvent> {
    Callback::from(move |_| {
        let status = status.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let browser = detect_browser();
            let Some(url) = debug_url(browser, target) else {
                status.set(format!(
                    "cannot detect supported browser for {}",
                    target.label()
                ));
                return;
            };
            match open_debug_url(url).await {
                Ok(()) => status.set(format!("opened {} {}", browser.label(), target.label())),
                Err(error) => status.set(format!(
                    "open {} {} failed: {error}",
                    browser.label(),
                    target.label()
                )),
            }
        });
    })
}

fn debug_url(browser: BrowserKind, target: DebugTarget) -> Option<&'static str> {
    match (browser, target) {
        (BrowserKind::Chrome, DebugTarget::WebRtc) => Some(CHROME_WEBRTC_DEBUG_URL),
        (BrowserKind::Chrome, DebugTarget::ExtensionManager) => Some(CHROME_EXTENSION_MANAGER_URL),
        (BrowserKind::Firefox, DebugTarget::WebRtc) => Some(FIREFOX_WEBRTC_DEBUG_URL),
        (BrowserKind::Firefox, DebugTarget::ExtensionManager) => {
            Some(FIREFOX_EXTENSION_MANAGER_URL)
        }
        (BrowserKind::Unknown, _) => None,
    }
}

fn detect_browser() -> BrowserKind {
    let user_agent = navigator_user_agent()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if user_agent.contains("firefox/") {
        BrowserKind::Firefox
    } else if user_agent.contains("chrome/") || user_agent.contains("chromium/") {
        BrowserKind::Chrome
    } else {
        BrowserKind::Unknown
    }
}

fn navigator_user_agent() -> Result<String, String> {
    let navigator =
        Reflect::get(&js_sys::global(), &JsValue::from_str("navigator")).map_err(js_error_label)?;
    js_string_field(&navigator, "userAgent")
}

fn link_control(
    state: ConnectState<'_>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    peers: UseStateHandle<Vec<PeerView>>,
    status: UseStateHandle<String>,
) -> Html {
    let on_http_connect = {
        let node_ref = node_ref.clone();
        let endpoint = (*state.http_endpoint).clone();
        let peers = peers.clone();
        let status = status.clone();
        let link_dialog_open = (*state.link_dialog_open).clone();
        Callback::from(move |_| {
            if let Some(bridge) = extension_node_bridge() {
                let endpoint = (*endpoint).clone();
                let peers = peers.clone();
                let status = status.clone();
                let link_dialog_open = link_dialog_open.clone();
                status.set(format!("connecting {endpoint}"));
                wasm_bindgen_futures::spawn_local(async move {
                    match extension_node_connect_http(&bridge, endpoint).await {
                        Ok(snapshot) => {
                            link_dialog_open.set(false);
                            peers.set(snapshot.peers);
                            status.set(snapshot.message);
                        }
                        Err(error) => status.set(error),
                    }
                });
                return;
            }

            let Some(node) = node_ref.borrow().clone() else {
                status.set("start the node first".to_string());
                return;
            };
            let endpoint = (*endpoint).clone();
            let peers = peers.clone();
            let status = status.clone();
            let link_dialog_open = link_dialog_open.clone();
            status.set(format!("connecting {endpoint}"));
            wasm_bindgen_futures::spawn_local(async move {
                match node::connect_http(&node.provider, endpoint).await {
                    Ok(seed_did) => {
                        link_dialog_open.set(false);
                        let seed_peer = PeerView {
                            did: seed_did,
                            state: "Connected".to_string(),
                        };
                        sync_peers_after_handshake(
                            node,
                            peers,
                            status,
                            "HTTP endpoint connected",
                            Some(seed_peer),
                        )
                        .await;
                    }
                    Err(error) => status.set(error),
                }
            });
        })
    };
    let on_create_offer = {
        let node_ref = node_ref.clone();
        let remote_did = (*state.sdp_remote_did).clone();
        let generated_offer = (*state.generated_offer).clone();
        let status = status.clone();
        Callback::from(move |_| {
            if let Some(bridge) = extension_node_bridge() {
                let remote_did = (*remote_did).trim().to_string();
                if remote_did.is_empty() {
                    status.set("enter a remote DID".to_string());
                    return;
                }
                let generated_offer = generated_offer.clone();
                let status = status.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match extension_node_create_offer(&bridge, remote_did).await {
                        Ok(offer) => {
                            generated_offer.set(offer);
                            status.set("offer created".to_string());
                        }
                        Err(error) => status.set(error),
                    }
                });
                return;
            }

            let Some(node) = node_ref.borrow().clone() else {
                status.set("start the node first".to_string());
                return;
            };
            let remote_did = (*remote_did).trim().to_string();
            if remote_did.is_empty() {
                status.set("enter a remote DID".to_string());
                return;
            }
            let generated_offer = generated_offer.clone();
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match node::create_offer(&node.provider, remote_did).await {
                    Ok(offer) => {
                        generated_offer.set(offer);
                        status.set("offer created".to_string());
                    }
                    Err(error) => status.set(error),
                }
            });
        })
    };
    let on_answer_offer = {
        let node_ref = node_ref.clone();
        let remote_offer = (*state.remote_offer).clone();
        let generated_answer = (*state.generated_answer).clone();
        let status = status.clone();
        Callback::from(move |_| {
            if let Some(bridge) = extension_node_bridge() {
                let offer = (*remote_offer).trim().to_string();
                if offer.is_empty() {
                    status.set("paste an offer first".to_string());
                    return;
                }
                let generated_answer = generated_answer.clone();
                let status = status.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match extension_node_answer_offer(&bridge, offer).await {
                        Ok(answer) => {
                            generated_answer.set(answer);
                            status.set("answer created".to_string());
                        }
                        Err(error) => status.set(error),
                    }
                });
                return;
            }

            let Some(node) = node_ref.borrow().clone() else {
                status.set("start the node first".to_string());
                return;
            };
            let offer = (*remote_offer).trim().to_string();
            if offer.is_empty() {
                status.set("paste an offer first".to_string());
                return;
            }
            let generated_answer = generated_answer.clone();
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match node::answer_offer(&node.provider, offer).await {
                    Ok(answer) => {
                        generated_answer.set(answer);
                        status.set("answer created".to_string());
                    }
                    Err(error) => status.set(error),
                }
            });
        })
    };
    let on_accept_answer = {
        let node_ref = node_ref.clone();
        let remote_answer = (*state.remote_answer).clone();
        let peers = peers.clone();
        let status = status.clone();
        let link_dialog_open = (*state.link_dialog_open).clone();
        Callback::from(move |_| {
            if let Some(bridge) = extension_node_bridge() {
                let answer = (*remote_answer).trim().to_string();
                if answer.is_empty() {
                    status.set("paste an answer first".to_string());
                    return;
                }
                let peers = peers.clone();
                let status = status.clone();
                let link_dialog_open = link_dialog_open.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match extension_node_accept_answer(&bridge, answer).await {
                        Ok(snapshot) => {
                            link_dialog_open.set(false);
                            peers.set(snapshot.peers);
                            status.set(snapshot.message);
                        }
                        Err(error) => status.set(error),
                    }
                });
                return;
            }

            let Some(node) = node_ref.borrow().clone() else {
                status.set("start the node first".to_string());
                return;
            };
            let answer = (*remote_answer).trim().to_string();
            if answer.is_empty() {
                status.set("paste an answer first".to_string());
                return;
            }
            let peers = peers.clone();
            let status = status.clone();
            let link_dialog_open = link_dialog_open.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match node::accept_answer(&node.provider, answer).await {
                    Ok(()) => {
                        link_dialog_open.set(false);
                        sync_peers_after_handshake(node, peers, status, "answer accepted", None)
                            .await;
                    }
                    Err(error) => status.set(error),
                }
            });
        })
    };
    let set_initiator = {
        let sdp_mode = (*state.sdp_mode).clone();
        Callback::from(move |_| sdp_mode.set(SdpMode::Initiator))
    };
    let set_responder = {
        let sdp_mode = (*state.sdp_mode).clone();
        Callback::from(move |_| sdp_mode.set(SdpMode::Responder))
    };
    let open_dialog = {
        let link_dialog_open = (*state.link_dialog_open).clone();
        Callback::from(move |_| link_dialog_open.set(true))
    };
    let close_dialog = {
        let link_dialog_open = (*state.link_dialog_open).clone();
        Callback::from(move |_| link_dialog_open.set(false))
    };
    let set_manual_sdp = {
        let link_tab = (*state.link_tab).clone();
        Callback::from(move |_| link_tab.set(LinkTab::ManualSdp))
    };
    let set_http_endpoint = {
        let link_tab = (*state.link_tab).clone();
        Callback::from(move |_| link_tab.set(LinkTab::HttpEndpoint))
    };

    html! {
        <div class="node-link-control">
            if !state.launcher_hidden {
                <button class="topology-add-button" type="button" aria-label="Connect peer" title="Connect peer" onclick={open_dialog}>
                    <span aria-hidden="true">{ "+" }</span>
                </button>
            }
            {
                if **state.link_dialog_open {
                    connect_dialog(
                        **state.link_tab,
                        **state.sdp_mode,
                        set_manual_sdp,
                        set_http_endpoint,
                        set_initiator,
                        set_responder,
                        close_dialog,
                        (*state.http_endpoint).clone(),
                        (*state.sdp_remote_did).clone(),
                        (*state.generated_offer).clone(),
                        (*state.remote_offer).clone(),
                        (*state.generated_answer).clone(),
                        (*state.remote_answer).clone(),
                        status.clone(),
                        on_http_connect,
                        on_create_offer,
                        on_answer_offer,
                        on_accept_answer,
                    )
                } else {
                    html! {}
                }
            }
        </div>
    }
}

#[allow(clippy::too_many_arguments)]
fn connect_dialog(
    active_tab: LinkTab,
    active_sdp_mode: SdpMode,
    set_manual_sdp: Callback<MouseEvent>,
    set_http_endpoint: Callback<MouseEvent>,
    set_initiator: Callback<MouseEvent>,
    set_responder: Callback<MouseEvent>,
    close_dialog: Callback<MouseEvent>,
    http_endpoint: UseStateHandle<String>,
    sdp_remote_did: UseStateHandle<String>,
    generated_offer: UseStateHandle<String>,
    remote_offer: UseStateHandle<String>,
    generated_answer: UseStateHandle<String>,
    remote_answer: UseStateHandle<String>,
    status: UseStateHandle<String>,
    on_http_connect: Callback<MouseEvent>,
    on_create_offer: Callback<MouseEvent>,
    on_answer_offer: Callback<MouseEvent>,
    on_accept_answer: Callback<MouseEvent>,
) -> Html {
    html! {
        <div class="modal-shell">
            <button class="dialog-backdrop" aria-label="Close link dialog" onclick={close_dialog.clone()}></button>
            <section class="link-dialog" role="dialog" aria-modal="true" aria-labelledby="link-dialog-title">
                <header class="dialog-header">
                    <div>
                        <p class="eyebrow">{ "Peer link" }</p>
                        <h2 id="link-dialog-title">{ "Connection exchange" }</h2>
                    </div>
                    <button class="secondary dialog-close" onclick={close_dialog}>{ "Close" }</button>
                </header>
                { link_dialog_tabs(active_tab, set_manual_sdp, set_http_endpoint) }
                <div class="dialog-body">
                    {
                        match active_tab {
                            LinkTab::ManualSdp => html! {
                                <div class="dialog-pane sdp-tool">
                                    <div class="tool-header">
                                        <h3>{ "Manual SDP exchange" }</h3>
                                        { sdp_mode_switch(active_sdp_mode, set_initiator, set_responder) }
                                    </div>
                                    {
                                        match active_sdp_mode {
                                            SdpMode::Initiator => sdp_initiator_flow(
                                                sdp_remote_did,
                                                generated_offer,
                                                remote_answer,
                                                status.clone(),
                                                on_create_offer,
                                                on_accept_answer,
                                            ),
                                            SdpMode::Responder => sdp_responder_flow(
                                                remote_offer,
                                                generated_answer,
                                                status.clone(),
                                                on_answer_offer,
                                            ),
                                        }
                                    }
                                </div>
                            },
                            LinkTab::HttpEndpoint => html! {
                                <div class="dialog-pane http-pane">
                                    <div class="tool-header">
                                        <h3>{ "HTTP endpoint" }</h3>
                                        <span class="payload-state">{ "Seed" }</span>
                                    </div>
                                    { text_input("Seed HTTP endpoint", http_endpoint) }
                                    <button onclick={on_http_connect}>{ "Connect endpoint" }</button>
                                </div>
                            },
                        }
                    }
                </div>
            </section>
        </div>
    }
}

fn link_dialog_tabs(
    active: LinkTab,
    set_manual_sdp: Callback<MouseEvent>,
    set_http_endpoint: Callback<MouseEvent>,
) -> Html {
    let manual_class = if active == LinkTab::ManualSdp {
        "dialog-tab active"
    } else {
        "dialog-tab"
    };
    let http_class = if active == LinkTab::HttpEndpoint {
        "dialog-tab active"
    } else {
        "dialog-tab"
    };
    html! {
        <nav class="dialog-tabs" aria-label="Connection mode">
            <button class={manual_class} onclick={set_manual_sdp}>{ "Manual SDP" }</button>
            <button class={http_class} onclick={set_http_endpoint}>{ "HTTP endpoint" }</button>
        </nav>
    }
}

fn sdp_mode_switch(
    active: SdpMode,
    set_initiator: Callback<MouseEvent>,
    set_responder: Callback<MouseEvent>,
) -> Html {
    let initiator_class = if active == SdpMode::Initiator {
        "segment active"
    } else {
        "segment"
    };
    let responder_class = if active == SdpMode::Responder {
        "segment active"
    } else {
        "segment"
    };
    html! {
        <div class="segmented" aria-label="SDP role">
            <button class={initiator_class} onclick={set_initiator}>{ "Initiator" }</button>
            <button class={responder_class} onclick={set_responder}>{ "Responder" }</button>
        </div>
    }
}

fn sdp_initiator_flow(
    remote_did: UseStateHandle<String>,
    generated_offer: UseStateHandle<String>,
    remote_answer: UseStateHandle<String>,
    status: UseStateHandle<String>,
    on_create_offer: Callback<MouseEvent>,
    on_accept_answer: Callback<MouseEvent>,
) -> Html {
    html! {
        <div class="sdp-flow">
            { sdp_step(
                "1",
                "Remote DID",
                html! {
                    <>
                        { text_input("Remote DID", remote_did) }
                        <button onclick={on_create_offer}>{ "Create offer" }</button>
                    </>
                },
            ) }
            { sdp_output_step("2", "Local offer", (*generated_offer).clone(), status) }
            { sdp_step(
                "3",
                "Remote answer",
                html! {
                    <>
                        { textarea("Remote answer", remote_answer) }
                        <button onclick={on_accept_answer}>{ "Accept answer" }</button>
                    </>
                },
            ) }
        </div>
    }
}

fn sdp_responder_flow(
    remote_offer: UseStateHandle<String>,
    generated_answer: UseStateHandle<String>,
    status: UseStateHandle<String>,
    on_answer_offer: Callback<MouseEvent>,
) -> Html {
    html! {
        <div class="sdp-flow">
            { sdp_step(
                "1",
                "Remote offer",
                html! {
                    <>
                        { textarea("Remote offer", remote_offer) }
                        <button onclick={on_answer_offer}>{ "Answer offer" }</button>
                    </>
                },
            ) }
            { sdp_output_step("2", "Local answer", (*generated_answer).clone(), status) }
        </div>
    }
}

fn sdp_step(index: &'static str, title: &'static str, body: Html) -> Html {
    html! {
        <div class="sdp-step">
            <div class="sdp-index">{ index }</div>
            <div class="sdp-step-body">
                <h4>{ title }</h4>
                { body }
            </div>
        </div>
    }
}

fn sdp_output_step(
    index: &'static str,
    title: &'static str,
    value: String,
    status: UseStateHandle<String>,
) -> Html {
    let can_copy = !value.trim().is_empty();
    let state = if value.trim().is_empty() {
        "Waiting"
    } else {
        "Ready"
    };
    let on_copy = {
        let value = value.clone();
        Callback::from(move |_| {
            if value.trim().is_empty() {
                status.set("generate SDP first".to_string());
                return;
            }
            let value = value.clone();
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match copy_text_to_clipboard(value).await {
                    Ok(()) => status.set(format!("{title} copied")),
                    Err(error) => status.set(format!("copy SDP failed: {error}")),
                }
            });
        })
    };
    html! {
        <div class="sdp-step">
            <div class="sdp-index">{ index }</div>
            <div class="sdp-step-body">
                <div class="sdp-output-header">
                    <h4>{ title }</h4>
                    <div class="sdp-output-actions">
                        <span class="payload-state">{ state }</span>
                        <button
                            class="copy-button sdp-copy"
                            type="button"
                            disabled={!can_copy}
                            onclick={on_copy}
                        >
                            { "Copy" }
                        </button>
                    </div>
                </div>
                { readonly_textarea(title, value) }
            </div>
        </div>
    }
}

fn dweb_panel(
    state: DwebState<'_>,
    site: Rc<RefCell<HashMap<String, String>>>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    status: UseStateHandle<String>,
) -> Html {
    let on_save = {
        let host_path = (*state.host_path).clone();
        let host_body = (*state.host_body).clone();
        let hosted_pages = (*state.hosted_pages).clone();
        let site = site.clone();
        let status = status.clone();
        Callback::from(move |_| {
            let path = (*host_path).trim().to_string();
            if path.is_empty() {
                status.set("path cannot be empty".to_string());
                return;
            }
            site.borrow_mut().insert(path.clone(), (*host_body).clone());
            let mut pages: Vec<_> = site
                .borrow()
                .iter()
                .map(|(path, body)| (path.clone(), body.clone()))
                .collect();
            pages.sort_by(|a, b| a.0.cmp(&b.0));
            hosted_pages.set(pages);
            status.set(format!("hosting {path}"));
        })
    };
    let on_fetch = {
        let node_ref = node_ref.clone();
        let peer = (*state.fetch_peer).clone();
        let path = (*state.fetch_path).clone();
        let status = status.clone();
        Callback::from(move |_| {
            let Some(node) = node_ref.borrow().clone() else {
                status.set("start the node first".to_string());
                return;
            };
            let peer = (*peer).trim().to_string();
            let path = (*path).trim().to_string();
            if peer.is_empty() || path.is_empty() {
                status.set("enter peer DID and path".to_string());
                return;
            }
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match dweb::fetch(node.provider.clone(), peer, path).await {
                    Ok(()) => status.set("dweb request sent".to_string()),
                    Err(error) => status.set(error),
                }
            });
        })
    };
    html! {
        <section class="feature-panel" id="dweb">
            <div class="section-heading">
                <p class="eyebrow">{ "Dweb" }</p>
                <h2>{ "Publish and resolve browser-hosted content" }</h2>
            </div>
            <div class="workflow-grid">
                <div class="tool-block">
                    <h3>{ "Publish" }</h3>
                    { text_input("Path", (*state.host_path).clone()) }
                    { textarea("HTML body", (*state.host_body).clone()) }
                    <button onclick={on_save}>{ "Save hosted page" }</button>
                    <div class="list">
                        { for state.hosted_pages.iter().map(|(path, body)| html! {
                            <div class="list-item">
                                <div class="mono">{ path.clone() }</div>
                                <div class="muted">{ format!("{} bytes", body.len()) }</div>
                            </div>
                        })}
                    </div>
                </div>
                <div class="tool-block">
                    <h3>{ "Resolve" }</h3>
                    { text_input("Peer DID", (*state.fetch_peer).clone()) }
                    { text_input("Path", (*state.fetch_path).clone()) }
                    <button onclick={on_fetch}>{ "Fetch page" }</button>
                    <iframe class="iframe" title="dweb page" sandbox="" srcdoc={(**state.dweb_page).clone()} />
                </div>
            </div>
        </section>
    }
}

fn proof_panel(
    prover_did: &UseStateHandle<String>,
    r1cs_url: &UseStateHandle<String>,
    wasm_url: &UseStateHandle<String>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    status: UseStateHandle<String>,
) -> Html {
    let on_prove = {
        let node_ref = node_ref.clone();
        let prover_did = prover_did.clone();
        let r1cs_url = r1cs_url.clone();
        let wasm_url = wasm_url.clone();
        let status = status.clone();
        Callback::from(move |_| {
            let Some(node) = node_ref.borrow().clone() else {
                status.set("start the node first".to_string());
                return;
            };
            let prover = (*prover_did).clone();
            let r1cs = (*r1cs_url).clone();
            let wasm = (*wasm_url).clone();
            let status = status.clone();
            status.set("offloading proof task".to_string());
            wasm_bindgen_futures::spawn_local(async move {
                match proof::run(node, prover, r1cs, wasm).await {
                    Ok(result) => status.set(proof::result_label(result).to_string()),
                    Err(error) => status.set(error),
                }
            });
        })
    };
    html! {
        <section class="feature-panel" id="proof">
            <div class="section-heading">
                <p class="eyebrow">{ "Proof demo" }</p>
                <h2>{ "Offload and verify proof work" }</h2>
            </div>
            <div class="proof-grid">
                <div class="tool-block">
                    { text_input("Prover DID", prover_did.clone()) }
                    { text_input("R1CS URL", r1cs_url.clone()) }
                    { text_input("WASM URL", wasm_url.clone()) }
                    <button onclick={on_prove}>{ "Generate proof" }</button>
                </div>
                <div class="proof-states">
                    { metric("Verified", proof::result_label(ProofResult::Verified).to_string()) }
                    { metric("Invalid", proof::result_label(ProofResult::Invalid).to_string()) }
                    { metric("Pending", proof::result_label(ProofResult::Pending).to_string()) }
                </div>
            </div>
        </section>
    }
}

fn custom_panel(
    namespace: &UseStateHandle<String>,
    registered: &UseStateHandle<Vec<String>>,
    peer: &UseStateHandle<String>,
    payload: &UseStateHandle<String>,
    events: &UseStateHandle<Vec<custom::CustomEvent>>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    status: UseStateHandle<String>,
) -> Html {
    let on_register = {
        let namespace = namespace.clone();
        let registered = registered.clone();
        let node_ref = node_ref.clone();
        let events = events.clone();
        let status = status.clone();
        Callback::from(move |_| {
            let ns = (*namespace).trim().to_string();
            if ns.is_empty() {
                status.set("namespace cannot be empty".to_string());
                return;
            }
            if registered.iter().any(|item| item == &ns) {
                status.set(format!("{ns} is already registered"));
                return;
            }
            let Some(node) = node_ref.borrow().clone() else {
                status.set("start the node first".to_string());
                return;
            };
            let on_custom = {
                let events = events.clone();
                Callback::from(move |event: custom::CustomEvent| {
                    let mut next = (*events).clone();
                    next.insert(0, event);
                    next.truncate(20);
                    events.set(next);
                })
            };
            match custom::register(&node.provider, ns.clone(), on_custom) {
                Ok(()) => {
                    let mut next = (*registered).clone();
                    next.push(ns.clone());
                    registered.set(next);
                    status.set(format!("registered {ns}"));
                }
                Err(error) => status.set(error),
            }
        })
    };
    let on_send = {
        let namespace = namespace.clone();
        let peer = peer.clone();
        let payload = payload.clone();
        let node_ref = node_ref.clone();
        let status = status.clone();
        Callback::from(move |_| {
            let Some(node) = node_ref.borrow().clone() else {
                status.set("start the node first".to_string());
                return;
            };
            let ns = (*namespace).trim().to_string();
            let did = (*peer).trim().to_string();
            if ns.is_empty() || did.is_empty() {
                status.set("enter namespace and destination DID".to_string());
                return;
            }
            let payload = (*payload).clone();
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match custom::send(node.provider.clone(), did, ns, payload).await {
                    Ok(()) => status.set("custom message sent".to_string()),
                    Err(error) => status.set(error),
                }
            });
        })
    };
    html! {
        <section class="feature-panel" id="custom">
            <div class="section-heading">
                <p class="eyebrow">{ "Custom messages" }</p>
                <h2>{ "Send protocol messages from the browser" }</h2>
            </div>
            <div class="workflow-grid">
                <div class="tool-block">
                    { text_input("Namespace", namespace.clone()) }
                    <button class="secondary" onclick={on_register}>{ "Register namespace" }</button>
                    { text_input("Destination DID", peer.clone()) }
                    { textarea("Payload", payload.clone()) }
                    <button onclick={on_send}>{ "Send custom message" }</button>
                    <p class="muted">{ format!("Registered: {}", registered.join(", ")) }</p>
                </div>
                <div class="tool-block">
                    <h3>{ "Inbound" }</h3>
                    <div class="list">
                        { for events.iter().map(|event| html! {
                            <div class="list-item">
                                <div><b>{ event.namespace.clone() }</b>{ " from " }<span class="mono">{ event.from.clone() }</span></div>
                                <div>{ event.payload.clone() }</div>
                            </div>
                        })}
                    </div>
                </div>
            </div>
        </section>
    }
}

async fn sync_peers_after_handshake(
    node: DemoNode,
    peers: UseStateHandle<Vec<PeerView>>,
    status: UseStateHandle<String>,
    context: &'static str,
    required_peer: Option<PeerView>,
) {
    status.set(format!("{context}; syncing peers"));
    if let Some(required_peer) = required_peer.as_ref() {
        peers.set(merge_required_peer((*peers).clone(), required_peer));
    }
    for delay_ms in [0, 1_000, 2_000, 4_000] {
        if delay_ms > 0 {
            sleep(Duration::from_millis(delay_ms)).await;
        }
        match node::list_peers(&node.provider).await {
            Ok(next) => {
                let next = if let Some(required_peer) = required_peer.as_ref() {
                    merge_required_peer(next, required_peer)
                } else {
                    next
                };
                let count = next.len();
                peers.set(next);
                status.set(peer_sync_status(context, count));
            }
            Err(error) => status.set(format!("{context}; peer sync failed: {error}")),
        }
    }
}

fn merge_required_peer(mut peers: Vec<PeerView>, required: &PeerView) -> Vec<PeerView> {
    if required.did.trim().is_empty() {
        return peers;
    }
    if !peers.iter().any(|peer| peer.did == required.did) {
        peers.insert(0, required.clone());
    }
    peers
}

fn peer_sync_status(context: &str, count: usize) -> String {
    match count {
        0 => format!("{context}; no peers visible yet"),
        1 => format!("{context}; 1 peer visible"),
        count => format!("{context}; {count} peers visible"),
    }
}

fn topology(did: &str, peers: &[PeerView]) -> Html {
    html! {
        <Topology did={did.to_string()} peers={peers.to_vec()} />
    }
}

#[derive(Properties, Clone, PartialEq)]
struct TopologyProps {
    did: String,
    peers: Vec<PeerView>,
}

#[function_component(Topology)]
fn topology_component(props: &TopologyProps) -> Html {
    let width = 420.0;
    let height = 420.0;
    let center_x = width / 2.0;
    let center_y = height / 2.0;
    let radius = 144.0;
    let nodes = chord_nodes(&props.did, &props.peers);
    let successor_edges = inferred_successor_edges(nodes.len());
    let finger_links = inferred_finger_links(&nodes);
    let local_context = local_chord_context(&nodes);
    let outer_orbit = open_orbit_path(center_x, center_y, radius + 42.0);
    let main_orbit = open_orbit_path(center_x, center_y, radius);
    let inner_orbit = open_orbit_path(center_x, center_y, radius - 50.0);
    let node_count = nodes.len();
    let show_remote_labels = node_count <= 6;
    let hovered_node = use_state(|| None::<String>);
    let pinned_node = use_state(|| None::<String>);
    let active_did = (*pinned_node).clone().or_else(|| (*hovered_node).clone());
    let clear_pinned = {
        let pinned_node = pinned_node.clone();
        Callback::from(move |_| pinned_node.set(None))
    };
    let clear_hovered = {
        let hovered_node = hovered_node.clone();
        Callback::from(move |_| hovered_node.set(None))
    };
    html! {
        <svg
            class="topology chord-topology"
            viewBox="0 0 420 420"
            role="img"
            aria-label="inferred Chord identifier ring"
            onclick={clear_pinned}
            onmouseleave={clear_hovered}
        >
            <path class="orbit outer" d={outer_orbit} />
            <path class="orbit" d={main_orbit.clone()} />
            <path class="orbit inner" d={inner_orbit} />
            <path class="scan" d={main_orbit} />
            <text class="topology-mode" x="20" y="30">{ "INFERRED CHORD RING" }</text>
            <text class="topology-count" x="20" y="48">{ format!("{node_count} visible IDs") }</text>
            <text class="ring-zero" x={center_x.to_string()} y="31" text-anchor="middle">{ "0 / 2^160" }</text>
            { for successor_edges.iter().map(|edge| {
                let source = &nodes[edge.source];
                let target = &nodes[edge.target];
                let class = if edge.target == 0 { "ring-edge wrap" } else { "ring-edge" };
                let flow_class = if edge.target == 0 { "data-flow ring-flow wrap" } else { "data-flow ring-flow" };
                let path = ring_arc_path(center_x, center_y, radius, source.angle, target.angle);
                html! {
                    <>
                        <path class={class} d={path.clone()}>
                            <title>{ format!("inferred successor: {} -> {}", source.did, target.did) }</title>
                        </path>
                        <path class={flow_class} d={path} aria-hidden="true" />
                    </>
                }
            })}
            { for finger_links.iter().map(|edge| {
                let source = &nodes[edge.source];
                let target = &nodes[edge.target];
                let tone = if source.is_local {
                    if edge.exponent == 159 { "primary" } else { "local" }
                } else {
                    "remote"
                };
                let class = format!("finger-link {tone}");
                let flow_class = format!("data-flow finger-flow {tone}");
                let flow_delay = format!(
                    "animation-delay: -{}ms;",
                    (edge.source * 311 + edge.exponent * 17) % 3600
                );
                let path = finger_curve_path(center_x, center_y, edge.exponent, source.angle, target.angle);
                html! {
                    <>
                        <path class={class} d={path.clone()}>
                            <title>{ format!("inferred finger 2^{}: {} -> {}", edge.exponent, source.did, target.did) }</title>
                        </path>
                        <path class={flow_class} d={path} style={flow_delay} aria-hidden="true" />
                    </>
                }
            })}
            <circle class="id-space-core" cx={center_x.to_string()} cy={center_y.to_string()} r="50" />
            <text class="core-label" x={center_x.to_string()} y={(center_y + 4.0).to_string()} text-anchor="middle">{ "RINGS" }</text>
            {
                if let Some((predecessor, successor)) = local_context {
                    html! {
                        <>
                            { ring_peer_label("predecessor-label", format!("PRED {predecessor}"), center_x, center_y, radius - 112.0) }
                            { ring_peer_label("successor-label", format!("SUCC {successor}"), center_x, center_y, radius - 99.0) }
                        </>
                    }
                } else {
                    html! {}
                }
            }
            { for nodes.iter().enumerate().map(|(index, node)| {
                let (x, y) = polar_point(center_x, center_y, radius, node.angle);
                let (label_x, label_y) = polar_point(center_x, center_y, radius + 31.0, node.angle);
                let node_class = node_class(node);
                let node_radius = node_radius(node, node_count);
                let index_label = if node.is_local { "L".to_string() } else { (index + 1).to_string() };
                let show_index = node.is_local || node_count <= 16;
                let show_label = node.is_local || show_remote_labels;
                let index_size = if node_count > 10 { "8" } else { "10" };
                let is_active = active_did.as_ref().is_some_and(|active| active == &node.did);
                let group_class = if is_active { "topology-node active" } else { "topology-node" };
                let hover_did = node.did.clone();
                let pin_did = node.did.clone();
                let on_mouse_enter = {
                    let hovered_node = hovered_node.clone();
                    Callback::from(move |_| hovered_node.set(Some(hover_did.clone())))
                };
                let on_mouse_leave = {
                    let hovered_node = hovered_node.clone();
                    Callback::from(move |_| hovered_node.set(None))
                };
                let on_click = {
                    let pinned_node = pinned_node.clone();
                    Callback::from(move |event: MouseEvent| {
                        event.stop_propagation();
                        pinned_node.set(Some(pin_did.clone()));
                    })
                };
                html! {
                    <g
                        class={group_class}
                        onmouseenter={on_mouse_enter}
                        onmouseleave={on_mouse_leave}
                        onclick={on_click}
                    >
                        <title>{ format!("{} {}", node.state, node.did) }</title>
                        <circle class={node_class} cx={svg_num(x)} cy={svg_num(y)} r={svg_num(node_radius)} />
                        {
                            if show_index {
                                html! {
                                    <text class="peer-index" x={svg_num(x)} y={svg_num(y + 3.5)} text-anchor="middle" font-size={index_size}>{ index_label }</text>
                                }
                            } else {
                                html! {}
                            }
                        }
                        {
                            if show_label {
                                html! {
                                    <text class={if node.is_local { "node-id local-id" } else { "node-id" }} x={svg_num(label_x)} y={svg_num(label_y)} text-anchor="middle" font-size="9">
                                        { short_did(&node.did) }
                                    </text>
                                }
                            } else {
                                html! {}
                            }
                        }
                    </g>
                }
            })}
            {
                active_did
                    .as_ref()
                    .and_then(|did| nodes.iter().find(|node| &node.did == did))
                    .map(|node| active_node_label(node, center_x, center_y, radius))
                    .unwrap_or_else(|| html! {})
            }
            {
                if nodes.is_empty() {
                    html! {
                        <text class="empty-node-label" x={center_x.to_string()} y={(center_y + radius + 38.0).to_string()} text-anchor="middle" font-size="11">
                            { "waiting for peers" }
                        </text>
                    }
                } else {
                    html! {}
                }
            }
        </svg>
    }
}

#[derive(Clone)]
struct ChordNode {
    did: String,
    state: String,
    id: [u8; 20],
    angle: f64,
    is_local: bool,
}

struct InferredEdge {
    source: usize,
    target: usize,
}

struct InferredFinger {
    source: usize,
    target: usize,
    exponent: usize,
}

fn chord_nodes(did: &str, peers: &[PeerView]) -> Vec<ChordNode> {
    let mut nodes = Vec::new();
    if let Some(id) = did_identifier(did) {
        nodes.push(ChordNode {
            did: did.to_string(),
            state: "local".to_string(),
            angle: chord_angle(&id),
            id,
            is_local: true,
        });
    }
    for peer in peers {
        if nodes.iter().any(|node| node.did == peer.did) {
            continue;
        }
        if let Some(id) = did_identifier(&peer.did) {
            nodes.push(ChordNode {
                did: peer.did.clone(),
                state: peer.state.clone(),
                angle: chord_angle(&id),
                id,
                is_local: false,
            });
        }
    }
    nodes.sort_by(|left, right| left.id.cmp(&right.id));
    nodes
}

fn inferred_successor_edges(node_count: usize) -> Vec<InferredEdge> {
    if node_count < 2 {
        return Vec::new();
    }
    (0..node_count)
        .map(|source| InferredEdge {
            source,
            target: (source + 1) % node_count,
        })
        .collect()
}

fn inferred_finger_links(nodes: &[ChordNode]) -> Vec<InferredFinger> {
    if nodes.len() < 4 {
        return Vec::new();
    }
    let exponents = if nodes.len() >= 16 {
        vec![159, 158, 157, 156]
    } else if nodes.len() >= 8 {
        vec![159, 158, 157, 156]
    } else {
        vec![159, 158, 157]
    };
    let mut links = Vec::new();
    for source in 0..nodes.len() {
        let mut source_targets = Vec::new();
        for exponent in &exponents {
            let target_id = chord_add_power_of_two(&nodes[source].id, *exponent);
            let target = first_successor_index(nodes, &target_id);
            if target == source || source_targets.contains(&target) {
                continue;
            }
            links.push(InferredFinger {
                source,
                target,
                exponent: *exponent,
            });
            source_targets.push(target);
        }
    }
    links
}

fn local_chord_context(nodes: &[ChordNode]) -> Option<(String, String)> {
    if nodes.len() < 2 {
        return None;
    }
    let local = nodes.iter().position(|node| node.is_local)?;
    let predecessor = if local == 0 {
        nodes.len() - 1
    } else {
        local - 1
    };
    let successor = (local + 1) % nodes.len();
    Some((nodes[predecessor].did.clone(), nodes[successor].did.clone()))
}

fn node_class(node: &ChordNode) -> &'static str {
    if node.is_local {
        return "ring-node local-node";
    }
    if node.state.eq_ignore_ascii_case("connected") {
        "ring-node peer-node connected"
    } else {
        "ring-node peer-node"
    }
}

fn node_radius(node: &ChordNode, node_count: usize) -> f64 {
    if node.is_local {
        return if node_count > 16 { 24.0 } else { 28.0 };
    }
    match node_count {
        0..=8 => 21.0,
        9..=16 => 15.0,
        _ => 10.0,
    }
}

fn first_successor_index(nodes: &[ChordNode], target_id: &[u8; 20]) -> usize {
    nodes
        .iter()
        .position(|node| node.id >= *target_id)
        .unwrap_or(0)
}

fn chord_add_power_of_two(id: &[u8; 20], exponent: usize) -> [u8; 20] {
    let mut out = *id;
    if exponent >= 160 {
        return out;
    }
    let byte_index = 19 - exponent / 8;
    let mut carry = (1u8 << (exponent % 8)) as u16;
    for byte in out[..=byte_index].iter_mut().rev() {
        let value = *byte as u16 + carry;
        *byte = value as u8;
        carry = value >> 8;
        if carry == 0 {
            break;
        }
    }
    out
}

fn did_identifier(did: &str) -> Option<[u8; 20]> {
    let hex = did.trim().strip_prefix("0x").unwrap_or(did.trim());
    if hex.is_empty() || hex.len() % 2 != 0 {
        return None;
    }
    let hex = if hex.len() > 40 {
        &hex[hex.len() - 40..]
    } else {
        hex
    };
    let byte_count = hex.len() / 2;
    if byte_count > 20 {
        return None;
    }
    let mut id = [0_u8; 20];
    let offset = 20 - byte_count;
    for index in 0..byte_count {
        let start = index * 2;
        let high = hex_nibble(hex.as_bytes()[start])?;
        let low = hex_nibble(hex.as_bytes()[start + 1])?;
        id[offset + index] = (high << 4) | low;
    }
    Some(id)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn chord_angle(id: &[u8; 20]) -> f64 {
    let mut fraction = 0.0;
    let mut scale = 1.0 / 256.0;
    for byte in id.iter().take(8) {
        fraction += *byte as f64 * scale;
        scale /= 256.0;
    }
    fraction * std::f64::consts::TAU - std::f64::consts::FRAC_PI_2
}

fn polar_point(center_x: f64, center_y: f64, radius: f64, angle: f64) -> (f64, f64) {
    (
        center_x + radius * angle.cos(),
        center_y + radius * angle.sin(),
    )
}

fn ring_arc_path(
    center_x: f64,
    center_y: f64,
    radius: f64,
    source_angle: f64,
    target_angle: f64,
) -> String {
    let (source_x, source_y) = polar_point(center_x, center_y, radius, source_angle);
    let (target_x, target_y) = polar_point(center_x, center_y, radius, target_angle);
    let large_arc = if clockwise_delta(source_angle, target_angle) > std::f64::consts::PI {
        1
    } else {
        0
    };
    format!(
        "M {} {} A {} {} 0 {} 1 {} {}",
        svg_num(source_x),
        svg_num(source_y),
        svg_num(radius),
        svg_num(radius),
        large_arc,
        svg_num(target_x),
        svg_num(target_y)
    )
}

fn open_orbit_path(center_x: f64, center_y: f64, radius: f64) -> String {
    let gap = 0.72;
    let top = -std::f64::consts::FRAC_PI_2;
    arc_path(
        center_x,
        center_y,
        radius,
        top + gap / 2.0,
        top - gap / 2.0 + std::f64::consts::TAU,
        true,
    )
}

fn arc_path(
    center_x: f64,
    center_y: f64,
    radius: f64,
    source_angle: f64,
    target_angle: f64,
    sweep: bool,
) -> String {
    let (source_x, source_y) = polar_point(center_x, center_y, radius, source_angle);
    let (target_x, target_y) = polar_point(center_x, center_y, radius, target_angle);
    let delta = if sweep {
        clockwise_delta(source_angle, target_angle)
    } else {
        clockwise_delta(target_angle, source_angle)
    };
    let large_arc = if delta > std::f64::consts::PI { 1 } else { 0 };
    let sweep_flag = if sweep { 1 } else { 0 };
    format!(
        "M {} {} A {} {} 0 {} {} {} {}",
        svg_num(source_x),
        svg_num(source_y),
        svg_num(radius),
        svg_num(radius),
        large_arc,
        sweep_flag,
        svg_num(target_x),
        svg_num(target_y)
    )
}

fn ring_peer_label(
    class_name: &'static str,
    label: String,
    center_x: f64,
    center_y: f64,
    radius: f64,
) -> Html {
    let chars: Vec<char> = label.chars().collect();
    let count = chars.len();
    if count == 0 {
        return html! {};
    }
    let start_angle = 2.78;
    let end_angle = 0.36;
    let denominator = if count > 1 { (count - 1) as f64 } else { 1.0 };
    let group_class = format!("ring-peer-label {class_name}");
    html! {
        <g class={group_class}>
            { for chars.into_iter().enumerate().map(|(index, ch)| {
                let t = index as f64 / denominator;
                let angle = start_angle + (end_angle - start_angle) * t;
                let (x, y) = polar_point(center_x, center_y, radius, angle);
                let rotation = (-angle.cos()).atan2(angle.sin()).to_degrees();
                let transform = format!(
                    "rotate({} {} {})",
                    svg_num(rotation),
                    svg_num(x),
                    svg_num(y)
                );
                html! {
                    <text x={svg_num(x)} y={svg_num(y)} text-anchor="middle" transform={transform}>
                        { ch }
                    </text>
                }
            }) }
        </g>
    }
}

fn active_node_label(node: &ChordNode, center_x: f64, center_y: f64, radius: f64) -> Html {
    let (node_x, node_y) = polar_point(center_x, center_y, radius, node.angle);
    let (raw_x, raw_y) = polar_point(center_x, center_y, radius + 64.0, node.angle);
    let readout_width = (node.did.chars().count() as f64 * 5.2 + 20.0).clamp(180.0, 340.0);
    let readout_x =
        (raw_x - readout_width / 2.0).clamp(18.0, center_x * 2.0 - readout_width - 18.0);
    let label_x = readout_x + readout_width / 2.0;
    let label_y = raw_y.clamp(66.0, center_y * 2.0 - 62.0);

    html! {
        <g class="active-node-readout" pointer-events="none">
            <line
                class="active-node-pointer"
                x1={svg_num(node_x)}
                y1={svg_num(node_y)}
                x2={svg_num(label_x)}
                y2={svg_num(label_y)}
            />
            <rect
                class="active-node-frame"
                x={svg_num(readout_x)}
                y={svg_num(label_y - 13.0)}
                width={svg_num(readout_width)}
                height="22"
                rx="4"
            />
            <text
                class="active-node-id"
                x={svg_num(label_x)}
                y={svg_num(label_y)}
                text-anchor="middle"
            >
                { node.did.clone() }
            </text>
        </g>
    }
}

fn finger_curve_path(
    center_x: f64,
    center_y: f64,
    exponent: usize,
    source_angle: f64,
    target_angle: f64,
) -> String {
    let radius = 144.0;
    let control_radius = match exponent {
        159 => 24.0,
        158 => 52.0,
        _ => 76.0,
    };
    let (source_x, source_y) = polar_point(center_x, center_y, radius - 10.0, source_angle);
    let (target_x, target_y) = polar_point(center_x, center_y, radius - 10.0, target_angle);
    let (control_1_x, control_1_y) = polar_point(center_x, center_y, control_radius, source_angle);
    let (control_2_x, control_2_y) = polar_point(center_x, center_y, control_radius, target_angle);
    format!(
        "M {} {} C {} {}, {} {}, {} {}",
        svg_num(source_x),
        svg_num(source_y),
        svg_num(control_1_x),
        svg_num(control_1_y),
        svg_num(control_2_x),
        svg_num(control_2_y),
        svg_num(target_x),
        svg_num(target_y)
    )
}

fn clockwise_delta(source_angle: f64, target_angle: f64) -> f64 {
    (target_angle - source_angle).rem_euclid(std::f64::consts::TAU)
}

fn svg_num(value: f64) -> String {
    format!("{value:.2}")
}

fn short_did(did: &str) -> String {
    if did.len() <= 14 {
        return did.to_string();
    }
    let prefix: String = did.chars().take(8).collect();
    let suffix: String = did
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}...{suffix}")
}

fn text_input(label: &'static str, state: UseStateHandle<String>) -> Html {
    let oninput = {
        let state = state.clone();
        Callback::from(move |event: InputEvent| {
            if let Some(value) = input_value(&event) {
                state.set(value);
            }
        })
    };
    html! {
        <label class="field">
            <span>{ label }</span>
            <input value={(*state).clone()} {oninput} />
        </label>
    }
}

fn textarea(label: &'static str, state: UseStateHandle<String>) -> Html {
    let oninput = {
        let state = state.clone();
        Callback::from(move |event: InputEvent| {
            if let Some(value) = textarea_value(&event) {
                state.set(value);
            }
        })
    };
    html! {
        <label class="field">
            <span>{ label }</span>
            <textarea value={(*state).clone()} {oninput} />
        </label>
    }
}

fn readonly_textarea(label: &'static str, value: String) -> Html {
    html! {
        <label class="field payload-output">
            <span>{ label }</span>
            <textarea readonly=true value={value} placeholder="Waiting for generated SDP" />
        </label>
    }
}

fn apply_extension_snapshot(
    snapshot: ExtensionNodeSnapshot,
    did: &UseStateHandle<String>,
    peers: &UseStateHandle<Vec<PeerView>>,
    wallet_account: &UseStateHandle<Option<WalletAccount>>,
    node_starting: &UseStateHandle<bool>,
    status: &UseStateHandle<String>,
) {
    node_starting.set(snapshot.starting);
    if snapshot.online {
        did.set(snapshot.did);
        peers.set(snapshot.peers);
        wallet_account.set(snapshot.wallet_account);
    }
    status.set(snapshot.error.unwrap_or(snapshot.message));
}

async fn poll_extension_node_start(
    bridge: &JsValue,
    did: UseStateHandle<String>,
    peers: UseStateHandle<Vec<PeerView>>,
    wallet_account: UseStateHandle<Option<WalletAccount>>,
    node_starting: UseStateHandle<bool>,
    status: UseStateHandle<String>,
) -> Result<(), String> {
    let mut last_message = "background node starting".to_string();
    for _attempt in 0..240 {
        sleep(Duration::from_millis(750)).await;
        let snapshot = extension_node_status(bridge).await?;
        let message = snapshot
            .error
            .clone()
            .unwrap_or_else(|| snapshot.message.clone());
        last_message = message.clone();
        let online = snapshot.online;
        let starting = snapshot.starting;
        let error = snapshot.error.clone();
        apply_extension_snapshot(
            snapshot,
            &did,
            &peers,
            &wallet_account,
            &node_starting,
            &status,
        );
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

fn load_setting(key: &str) -> Option<String> {
    let storage = local_storage().ok()?;
    let get_item = js_method(&storage, "getItem").ok()?;
    get_item
        .call1(&storage, &JsValue::from_str(key))
        .ok()
        .and_then(|value| value.as_string())
}

fn save_setting(key: &str, value: &str) {
    let Some(storage) = local_storage().ok() else {
        return;
    };
    let Some(set_item) = js_method(&storage, "setItem").ok() else {
        return;
    };
    let _stored = set_item.call2(&storage, &JsValue::from_str(key), &JsValue::from_str(value));
}

fn local_storage() -> Result<JsValue, String> {
    let storage = Reflect::get(&js_sys::global(), &JsValue::from_str("localStorage"))
        .map_err(js_error_label)?;
    if storage.is_null() || storage.is_undefined() {
        Err("localStorage unavailable".to_string())
    } else {
        Ok(storage)
    }
}

fn extension_node_bridge() -> Option<JsValue> {
    let bridge = Reflect::get(&js_sys::global(), &JsValue::from_str(EXTENSION_NODE_BRIDGE)).ok()?;
    if bridge.is_null()
        || bridge.is_undefined()
        || !is_callable(&bridge, "start")
        || !is_callable(&bridge, "stop")
        || !is_callable(&bridge, "status")
        || !is_callable(&bridge, "connectHttp")
    {
        return None;
    }
    Some(bridge)
}

async fn extension_node_start(
    bridge: &JsValue,
    kind: WalletKind,
    settings: ExtensionStartSettings,
) -> Result<ExtensionNodeSnapshot, String> {
    let settings = settings.to_js(kind)?;
    let result = call_extension_bridge1(bridge, "start", &settings).await?;
    parse_extension_node_snapshot(&result, bridge)
}

async fn extension_node_status(bridge: &JsValue) -> Result<ExtensionNodeSnapshot, String> {
    let result = call_extension_bridge0(bridge, "status").await?;
    parse_extension_node_snapshot(&result, bridge)
}

async fn extension_node_stop(bridge: &JsValue) -> Result<String, String> {
    let result = call_extension_bridge0(bridge, "stop").await?;
    let snapshot = parse_extension_node_snapshot(&result, bridge)?;
    Ok(snapshot.message)
}

async fn extension_node_connect_http(
    bridge: &JsValue,
    endpoint: String,
) -> Result<ExtensionNodeSnapshot, String> {
    let result =
        call_extension_bridge1(bridge, "connectHttp", &JsValue::from_str(&endpoint)).await?;
    parse_extension_node_snapshot(&result, bridge)
}

async fn extension_node_create_offer(bridge: &JsValue, did: String) -> Result<String, String> {
    let result = call_extension_bridge1(bridge, "createOffer", &JsValue::from_str(&did)).await?;
    js_string_field(&result, "offer")
}

async fn extension_node_answer_offer(bridge: &JsValue, offer: String) -> Result<String, String> {
    let result = call_extension_bridge1(bridge, "answerOffer", &JsValue::from_str(&offer)).await?;
    js_string_field(&result, "answer")
}

async fn extension_node_accept_answer(
    bridge: &JsValue,
    answer: String,
) -> Result<ExtensionNodeSnapshot, String> {
    let result =
        call_extension_bridge1(bridge, "acceptAnswer", &JsValue::from_str(&answer)).await?;
    parse_extension_node_snapshot(&result, bridge)
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
        let did = js_string_field(&peer, "did").unwrap_or_else(|_| "unknown".to_string());
        let state = js_string_field(&peer, "state").unwrap_or_else(|_| "Unknown".to_string());
        out.push(PeerView { did, state });
    }
    Ok(out)
}

async fn await_js(value: JsValue) -> Result<JsValue, String> {
    JsFuture::from(Promise::from(value))
        .await
        .map_err(js_error_label)
}

fn chrome_runtime_on_message() -> Option<JsValue> {
    let runtime = chrome_runtime()?;
    let on_message = js_prop(&runtime, "onMessage").ok()?;
    if on_message.is_null() || on_message.is_undefined() {
        None
    } else {
        Some(on_message)
    }
}

fn chrome_runtime() -> Option<JsValue> {
    let chrome = Reflect::get(&js_sys::global(), &JsValue::from_str("chrome")).ok()?;
    let runtime = js_prop(&chrome, "runtime").ok()?;
    if runtime.is_null() || runtime.is_undefined() {
        None
    } else {
        Some(runtime)
    }
}

fn is_callable(object: &JsValue, name: &str) -> bool {
    js_prop(object, name)
        .ok()
        .and_then(|value| value.dyn_into::<Function>().ok())
        .is_some()
}

fn js_method(object: &JsValue, name: &str) -> Result<Function, String> {
    js_prop(object, name)?
        .dyn_into::<Function>()
        .map_err(|_| format!("{name} is not callable"))
}

fn js_prop(object: &JsValue, name: &str) -> Result<JsValue, String> {
    Reflect::get(object, &JsValue::from_str(name)).map_err(js_error_label)
}

fn js_string_field(object: &JsValue, name: &str) -> Result<String, String> {
    js_prop(object, name)?
        .as_string()
        .ok_or_else(|| format!("missing string field {name}"))
}

fn js_bool_field(object: &JsValue, name: &str) -> Result<bool, String> {
    js_prop(object, name)?
        .as_bool()
        .ok_or_else(|| format!("missing bool field {name}"))
}

fn js_set(object: &Object, name: &str, value: &JsValue) -> Result<(), String> {
    Reflect::set(object, &JsValue::from_str(name), value)
        .map(|_| ())
        .map_err(js_error_label)
}

async fn copy_text_to_clipboard(value: String) -> Result<(), String> {
    let navigator =
        Reflect::get(&js_sys::global(), &JsValue::from_str("navigator")).map_err(js_error_label)?;
    let clipboard =
        Reflect::get(&navigator, &JsValue::from_str("clipboard")).map_err(js_error_label)?;
    if clipboard.is_null() || clipboard.is_undefined() {
        return Err("clipboard API unavailable".to_string());
    }
    let write_text =
        Reflect::get(&clipboard, &JsValue::from_str("writeText")).map_err(js_error_label)?;
    let write_text = write_text
        .dyn_into::<Function>()
        .map_err(|_| "clipboard.writeText unavailable".to_string())?;
    let promise = write_text
        .call1(&clipboard, &JsValue::from_str(&value))
        .map_err(js_error_label)?
        .dyn_into::<Promise>()
        .map_err(|_| "clipboard.writeText did not return a promise".to_string())?;
    JsFuture::from(promise).await.map_err(js_error_label)?;
    Ok(())
}

async fn open_debug_url(url: &str) -> Result<(), String> {
    match open_debug_url_with_extension_tabs("browser", url).await {
        Ok(()) => Ok(()),
        Err(_) => match open_debug_url_with_extension_tabs("chrome", url).await {
            Ok(()) => Ok(()),
            Err(_) => open_debug_url_with_window(url),
        },
    }
}

async fn open_debug_url_with_extension_tabs(namespace: &str, url: &str) -> Result<(), String> {
    let extension_api =
        Reflect::get(&js_sys::global(), &JsValue::from_str(namespace)).map_err(js_error_label)?;
    if extension_api.is_null() || extension_api.is_undefined() {
        return Err(format!("{namespace} extension API unavailable"));
    }
    let tabs = Reflect::get(&extension_api, &JsValue::from_str("tabs")).map_err(js_error_label)?;
    if tabs.is_null() || tabs.is_undefined() {
        return Err(format!("{namespace}.tabs unavailable"));
    }
    let create = Reflect::get(&tabs, &JsValue::from_str("create")).map_err(js_error_label)?;
    let create = create
        .dyn_into::<Function>()
        .map_err(|_| format!("{namespace}.tabs.create unavailable"))?;
    let options = js_sys::Object::new();
    Reflect::set(&options, &JsValue::from_str("url"), &JsValue::from_str(url))
        .map_err(js_error_label)?;
    let opened = create
        .call1(&tabs, &options.into())
        .map_err(js_error_label)?;
    if let Ok(promise) = opened.dyn_into::<Promise>() {
        JsFuture::from(promise).await.map_err(js_error_label)?;
    }
    Ok(())
}

fn open_debug_url_with_window(url: &str) -> Result<(), String> {
    let window =
        Reflect::get(&js_sys::global(), &JsValue::from_str("window")).map_err(js_error_label)?;
    if window.is_null() || window.is_undefined() {
        return Err("window unavailable".to_string());
    }
    let open = Reflect::get(&window, &JsValue::from_str("open")).map_err(js_error_label)?;
    let open = open
        .dyn_into::<Function>()
        .map_err(|_| "window.open unavailable".to_string())?;
    let opened = open
        .call2(
            &window,
            &JsValue::from_str(url),
            &JsValue::from_str("_blank"),
        )
        .map_err(js_error_label)?;
    if opened.is_null() || opened.is_undefined() {
        return Err("browser blocked the debug console tab".to_string());
    }
    Ok(())
}

fn js_error_label(error: JsValue) -> String {
    error.as_string().unwrap_or_else(|| format!("{error:?}"))
}

fn input_value(event: &InputEvent) -> Option<String> {
    event
        .target()
        .and_then(|target| target.dyn_into::<HtmlInputElement>().ok())
        .map(|input| input.value())
}

fn textarea_value(event: &InputEvent) -> Option<String> {
    event
        .target()
        .and_then(|target| target.dyn_into::<HtmlTextAreaElement>().ok())
        .map(|input| input.value())
}

fn select_value(event: &Event) -> Option<String> {
    event
        .target()
        .and_then(|target| target.dyn_into::<HtmlSelectElement>().ok())
        .map(|select| select.value())
}
