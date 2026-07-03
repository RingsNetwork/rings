//! Workbench panels for dweb, proof, and custom messages.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rings_node::extension::snark::ProofResult;
use yew::prelude::*;

use crate::controls::metric;
use crate::custom;
use crate::dweb;
use crate::forms::text_input;
use crate::forms::textarea;
use crate::node::DemoNode;
use crate::proof;

pub(crate) struct DwebState<'a> {
    pub(crate) host_path: &'a UseStateHandle<String>,
    pub(crate) host_body: &'a UseStateHandle<String>,
    pub(crate) hosted_pages: &'a UseStateHandle<Vec<(String, String)>>,
    pub(crate) fetch_peer: &'a UseStateHandle<String>,
    pub(crate) fetch_path: &'a UseStateHandle<String>,
    pub(crate) dweb_page: &'a UseStateHandle<String>,
}

pub(crate) fn dweb_panel(
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

pub(crate) fn proof_panel(
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

pub(crate) fn custom_panel(
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
