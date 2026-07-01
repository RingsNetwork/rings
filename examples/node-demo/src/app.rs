//! Yew application for the unified node demo.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rings_node::extension::snark::ProofResult;
use wasm_bindgen::JsCast;
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
    Connect,
    Dweb,
    Proof,
    Custom,
}

impl Panel {
    fn label(self) -> &'static str {
        match self {
            Self::Connect => "Connect",
            Self::Dweb => "Dweb",
            Self::Proof => "Proof",
            Self::Custom => "Custom",
        }
    }
}

/// Unified Rings node demo app.
#[function_component(App)]
pub fn app() -> Html {
    let active_panel = use_state(|| Panel::Connect);
    let wallet_kind = use_state(|| WalletKind::WebCrypto);
    let wallet_account = use_state(|| None::<WalletAccount>);
    let node_ref = use_mut_ref(|| None::<DemoNode>);
    let site = use_mut_ref(default_site);

    let did = use_state(String::new);
    let status = use_state(|| "select an account provider and start the browser node".to_string());
    let network_id = use_state(|| "1".to_string());
    let ice_servers = use_state(|| "stun://stun.l.google.com:19302".to_string());
    let stabilize_interval = use_state(|| "3".to_string());
    let storage_name = use_state(|| "rings-node-demo".to_string());
    let peers = use_state(Vec::<PeerView>::new);

    let http_endpoint = use_state(|| "http://127.0.0.1:50001".to_string());
    let sdp_remote_did = use_state(String::new);
    let generated_offer = use_state(String::new);
    let remote_offer = use_state(String::new);
    let generated_answer = use_state(String::new);
    let remote_answer = use_state(String::new);

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

    let on_start = {
        let wallet_kind = wallet_kind.clone();
        let wallet_account = wallet_account.clone();
        let node_ref = node_ref.clone();
        let site = site.clone();
        let did = did.clone();
        let status = status.clone();
        let network_id = network_id.clone();
        let ice_servers = ice_servers.clone();
        let stabilize_interval = stabilize_interval.clone();
        let storage_name = storage_name.clone();
        let dweb_page = dweb_page.clone();
        let custom_events = custom_events.clone();
        Callback::from(move |_| {
            let status = status.clone();
            let wallet_account = wallet_account.clone();
            let node_ref = node_ref.clone();
            let site = site.clone();
            let did = did.clone();
            let network_id = (*network_id).clone();
            let ice_servers = (*ice_servers).clone();
            let stabilize_interval = (*stabilize_interval).clone();
            let storage_name = (*storage_name).clone();
            let dweb_page = dweb_page.clone();
            let custom_events = custom_events.clone();
            let kind = *wallet_kind;
            status.set(format!("connecting {}", kind.label()));
            wasm_bindgen_futures::spawn_local(async move {
                let settings = match node_settings(
                    network_id,
                    ice_servers,
                    stabilize_interval,
                    storage_name,
                ) {
                    Ok(settings) => settings,
                    Err(error) => {
                        status.set(error);
                        return;
                    }
                };
                let account = match wallet::connect(kind).await {
                    Ok(account) => account,
                    Err(error) => {
                        status.set(error);
                        return;
                    }
                };
                status.set("authorizing session key".to_string());
                let built = match node::build_node(&account, settings).await {
                    Ok(node) => node,
                    Err(error) => {
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
                        status.set(error);
                        return;
                    }
                }
                did.set(my_did);
                wallet_account.set(Some(account));
                *node_ref.borrow_mut() = Some(built);
                status.set("node ready".to_string());
            });
        })
    };

    let refresh_peers = refresh_peers_callback(node_ref.clone(), peers.clone(), status.clone());

    html! {
        <main>
            <style>{ styles::APP_CSS }</style>
            <h1>{ "Rings Node Demo" }</h1>
            <p class="muted">{ "A unified browser node console for wallet login, SDP/HTTP connectivity, topology, dweb, proofs, and custom messages." }</p>
            <div class="shell">
                <aside class="side">
                    <h2>{ "Node" }</h2>
                    { wallet_controls(*wallet_kind, on_wallet_kind, on_start) }
                    { settings_controls(&network_id, &ice_servers, &stabilize_interval, &storage_name) }
                    <p class="status">{ (*status).clone() }</p>
                    <p class="muted">{ "DID" }</p>
                    <p class="mono">{ if (*did).is_empty() { "not started".to_string() } else { (*did).clone() } }</p>
                    { account_view((*wallet_account).clone()) }
                    <button class="secondary" onclick={refresh_peers.clone()}>{ "Refresh peers" }</button>
                    { peer_list(&peers) }
                </aside>
                <section class="panel">
                    { tabs(*active_panel, active_panel.clone()) }
                    {
                        match *active_panel {
                            Panel::Connect => connect_panel(
                                &did,
                                &peers,
                                &http_endpoint,
                                &sdp_remote_did,
                                &generated_offer,
                                &remote_offer,
                                &generated_answer,
                                &remote_answer,
                                node_ref.clone(),
                                status.clone(),
                            ),
                            Panel::Dweb => dweb_panel(
                                &host_path,
                                &host_body,
                                &hosted_pages,
                                &fetch_peer,
                                &fetch_path,
                                &dweb_page,
                                site.clone(),
                                node_ref.clone(),
                                status.clone(),
                            ),
                            Panel::Proof => proof_panel(
                                &prover_did,
                                &r1cs_url,
                                &wasm_url,
                                node_ref.clone(),
                                status.clone(),
                            ),
                            Panel::Custom => custom_panel(
                                &custom_namespace,
                                &custom_registered,
                                &custom_peer,
                                &custom_payload,
                                &custom_events,
                                node_ref.clone(),
                                status.clone(),
                            ),
                        }
                    }
                </section>
            </div>
        </main>
    }
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

fn tabs(active: Panel, active_panel: UseStateHandle<Panel>) -> Html {
    html! {
        <nav class="tabs">
            { for [Panel::Connect, Panel::Dweb, Panel::Proof, Panel::Custom].into_iter().map(|panel| {
                let active_panel = active_panel.clone();
                let class = if panel == active { "tab active" } else { "tab" };
                html! {
                    <button class={class} onclick={Callback::from(move |_| active_panel.set(panel))}>
                        { panel.label() }
                    </button>
                }
            })}
        </nav>
    }
}

fn wallet_controls(
    wallet_kind: WalletKind,
    on_wallet_kind: Callback<Event>,
    on_start: Callback<MouseEvent>,
) -> Html {
    html! {
        <>
            <label class="field">
                <span>{ "Account provider" }</span>
                <select onchange={on_wallet_kind} value={wallet_kind.value()}>
                    <option value="webcrypto">{ "WebCrypto P-256" }</option>
                    <option value="metamask">{ "MetaMask" }</option>
                    <option value="phantom">{ "Phantom" }</option>
                </select>
            </label>
            <button onclick={on_start}>{ "Start node" }</button>
        </>
    }
}

fn settings_controls(
    network_id: &UseStateHandle<String>,
    ice_servers: &UseStateHandle<String>,
    stabilize_interval: &UseStateHandle<String>,
    storage_name: &UseStateHandle<String>,
) -> Html {
    html! {
        <>
            { text_input("Network ID", network_id.clone()) }
            { text_input("ICE servers", ice_servers.clone()) }
            { text_input("Stabilize interval seconds", stabilize_interval.clone()) }
            { text_input("IndexedDB storage", storage_name.clone()) }
        </>
    }
}

fn account_view(account: Option<WalletAccount>) -> Html {
    match account {
        Some(account) => html! {
            <div class="list">
                <div class="list-item">
                    <div class="muted">{ "Account type" }</div>
                    <div>{ account.account_type }</div>
                </div>
                <div class="list-item">
                    <div class="muted">{ "Account" }</div>
                    <div class="mono">{ account.account }</div>
                </div>
            </div>
        },
        None => html! {},
    }
}

fn peer_list(peers: &UseStateHandle<Vec<PeerView>>) -> Html {
    html! {
        <div class="list">
            { for peers.iter().map(|peer| html! {
                <div class="list-item">
                    <div class="mono">{ peer.did.clone() }</div>
                    <div class="muted">{ peer.state.clone() }</div>
                </div>
            })}
        </div>
    }
}

fn connect_panel(
    did: &UseStateHandle<String>,
    peers: &UseStateHandle<Vec<PeerView>>,
    http_endpoint: &UseStateHandle<String>,
    sdp_remote_did: &UseStateHandle<String>,
    generated_offer: &UseStateHandle<String>,
    remote_offer: &UseStateHandle<String>,
    generated_answer: &UseStateHandle<String>,
    remote_answer: &UseStateHandle<String>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    status: UseStateHandle<String>,
) -> Html {
    let on_http_connect = {
        let node_ref = node_ref.clone();
        let endpoint = http_endpoint.clone();
        let status = status.clone();
        Callback::from(move |_| {
            let Some(node) = node_ref.borrow().clone() else {
                status.set("start the node first".to_string());
                return;
            };
            let endpoint = (*endpoint).clone();
            let status = status.clone();
            status.set(format!("connecting {endpoint}"));
            wasm_bindgen_futures::spawn_local(async move {
                match node::connect_http(&node.provider, endpoint).await {
                    Ok(()) => {
                        status.set("HTTP endpoint connected; refresh peers when ready".to_string());
                    }
                    Err(error) => status.set(error),
                }
            });
        })
    };
    let on_create_offer = {
        let node_ref = node_ref.clone();
        let remote_did = sdp_remote_did.clone();
        let generated_offer = generated_offer.clone();
        let status = status.clone();
        Callback::from(move |_| {
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
        let remote_offer = remote_offer.clone();
        let generated_answer = generated_answer.clone();
        let status = status.clone();
        Callback::from(move |_| {
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
        let remote_answer = remote_answer.clone();
        let status = status.clone();
        Callback::from(move |_| {
            let Some(node) = node_ref.borrow().clone() else {
                status.set("start the node first".to_string());
                return;
            };
            let answer = (*remote_answer).trim().to_string();
            if answer.is_empty() {
                status.set("paste an answer first".to_string());
                return;
            }
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match node::accept_answer(&node.provider, answer).await {
                    Ok(()) => {
                        status.set("answer accepted; refresh peers when ready".to_string());
                    }
                    Err(error) => status.set(error),
                }
            });
        })
    };

    html! {
        <>
            <h2>{ "Connectivity" }</h2>
            { topology((*did).as_str(), &*peers) }
            <div class="grid">
                <div>
                    <h3>{ "HTTP endpoint" }</h3>
                    { text_input("Seed HTTP endpoint", http_endpoint.clone()) }
                    <button onclick={on_http_connect}>{ "Connect endpoint" }</button>
                </div>
                <div>
                    <h3>{ "SDP exchange" }</h3>
                    { text_input("Remote DID", sdp_remote_did.clone()) }
                    <button onclick={on_create_offer}>{ "Create offer" }</button>
                    { textarea("Generated offer", generated_offer.clone()) }
                    { textarea("Remote offer", remote_offer.clone()) }
                    <button onclick={on_answer_offer}>{ "Answer offer" }</button>
                    { textarea("Generated answer", generated_answer.clone()) }
                    { textarea("Remote answer", remote_answer.clone()) }
                    <button onclick={on_accept_answer}>{ "Accept answer" }</button>
                </div>
            </div>
        </>
    }
}

fn dweb_panel(
    host_path: &UseStateHandle<String>,
    host_body: &UseStateHandle<String>,
    hosted_pages: &UseStateHandle<Vec<(String, String)>>,
    fetch_peer: &UseStateHandle<String>,
    fetch_path: &UseStateHandle<String>,
    dweb_page: &UseStateHandle<String>,
    site: Rc<RefCell<HashMap<String, String>>>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    status: UseStateHandle<String>,
) -> Html {
    let on_save = {
        let host_path = host_path.clone();
        let host_body = host_body.clone();
        let hosted_pages = hosted_pages.clone();
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
        let peer = fetch_peer.clone();
        let path = fetch_path.clone();
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
        <>
            <h2>{ "Dweb" }</h2>
            <div class="grid">
                <div>
                    <h3>{ "Host" }</h3>
                    { text_input("Path", host_path.clone()) }
                    { textarea("HTML body", host_body.clone()) }
                    <button onclick={on_save}>{ "Save hosted page" }</button>
                    <div class="list">
                        { for hosted_pages.iter().map(|(path, body)| html! {
                            <div class="list-item">
                                <div class="mono">{ path.clone() }</div>
                                <div class="muted">{ format!("{} bytes", body.len()) }</div>
                            </div>
                        })}
                    </div>
                </div>
                <div>
                    <h3>{ "Fetch" }</h3>
                    { text_input("Peer DID", fetch_peer.clone()) }
                    { text_input("Path", fetch_path.clone()) }
                    <button onclick={on_fetch}>{ "Fetch page" }</button>
                    <iframe class="iframe" title="dweb page" sandbox="" srcdoc={(**dweb_page).clone()} />
                </div>
            </div>
        </>
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
        <>
            <h2>{ "Proof demo" }</h2>
            <p class="muted">{ "Runs the existing distributed SNARK flow: load circuit, send proof task to a prover peer, then poll for verification result." }</p>
            { text_input("Prover DID", prover_did.clone()) }
            { text_input("R1CS URL", r1cs_url.clone()) }
            { text_input("WASM URL", wasm_url.clone()) }
            <button onclick={on_prove}>{ "Generate and send proof" }</button>
            <p class="muted">{ format!("Sample input result states: {}, {}, {}", proof::result_label(ProofResult::Verified), proof::result_label(ProofResult::Invalid), proof::result_label(ProofResult::Pending)) }</p>
        </>
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
        <>
            <h2>{ "Custom messages" }</h2>
            <div class="grid">
                <div>
                    { text_input("Namespace", namespace.clone()) }
                    <button class="secondary" onclick={on_register}>{ "Register namespace" }</button>
                    { text_input("Destination DID", peer.clone()) }
                    { textarea("Payload", payload.clone()) }
                    <button onclick={on_send}>{ "Send custom message" }</button>
                    <p class="muted">{ format!("Registered: {}", registered.join(", ")) }</p>
                </div>
                <div>
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
        </>
    }
}

fn refresh_peers_callback(
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    peers: UseStateHandle<Vec<PeerView>>,
    status: UseStateHandle<String>,
) -> Callback<MouseEvent> {
    Callback::from(move |_| {
        let Some(node) = node_ref.borrow().clone() else {
            status.set("start the node first".to_string());
            return;
        };
        let peers = peers.clone();
        let status = status.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match node::list_peers(&node.provider).await {
                Ok(next) => {
                    peers.set(next);
                    status.set("peers refreshed".to_string());
                }
                Err(error) => status.set(error),
            }
        });
    })
}

fn topology(did: &str, peers: &[PeerView]) -> Html {
    let width = 620.0;
    let height = 280.0;
    let center_x = width / 2.0;
    let center_y = height / 2.0;
    let radius = 96.0;
    let count = peers.len().max(1) as f64;
    html! {
        <svg class="topology" viewBox="0 0 620 280" role="img" aria-label="circular node topology">
            <circle cx={center_x.to_string()} cy={center_y.to_string()} r="36" fill="#0b63ce" />
            <text x={center_x.to_string()} y={(center_y + 5.0).to_string()} text-anchor="middle" fill="#fff" font-size="13">{ "local" }</text>
            <text x={center_x.to_string()} y={(center_y + 58.0).to_string()} text-anchor="middle" fill="#17202a" font-size="11">{ short_did(did) }</text>
            { for peers.iter().enumerate().map(|(index, peer)| {
                let angle = std::f64::consts::TAU * (index as f64) / count - std::f64::consts::FRAC_PI_2;
                let x = center_x + radius * angle.cos();
                let y = center_y + radius * angle.sin();
                let fill = if peer.state.eq_ignore_ascii_case("connected") { "#1f9d55" } else { "#8a98a8" };
                html! {
                    <>
                        <line x1={center_x.to_string()} y1={center_y.to_string()} x2={x.to_string()} y2={y.to_string()} stroke="#b8c2cc" />
                        <circle cx={x.to_string()} cy={y.to_string()} r="27" fill={fill} />
                        <text x={x.to_string()} y={(y + 4.0).to_string()} text-anchor="middle" fill="#fff" font-size="11">{ (index + 1).to_string() }</text>
                        <text x={x.to_string()} y={(y + 42.0).to_string()} text-anchor="middle" fill="#17202a" font-size="10">{ short_did(&peer.did) }</text>
                    </>
                }
            })}
        </svg>
    }
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
