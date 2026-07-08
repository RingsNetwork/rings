//! Workbench panels for dweb, proof, and custom messages.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rings_node::extension::snark::ProofResult;
use wasm_bindgen::JsCast;
use web_sys::Event;
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::controls::metric;
use crate::custom;
use crate::dweb;
use crate::extension;
use crate::forms::text_input;
use crate::forms::textarea;
use crate::node::DemoNode;
use crate::onion;
use crate::proof;

pub(crate) struct DwebState<'a> {
    pub(crate) host_path: &'a UseStateHandle<String>,
    pub(crate) host_body: &'a UseStateHandle<String>,
    pub(crate) hosted_pages: &'a UseStateHandle<Vec<(String, String)>>,
    pub(crate) fetch_peer: &'a UseStateHandle<String>,
    pub(crate) fetch_path: &'a UseStateHandle<String>,
    pub(crate) dweb_page: &'a UseStateHandle<String>,
}

pub(crate) struct OnionProxyState<'a> {
    pub(crate) url: &'a UseStateHandle<String>,
    pub(crate) method: &'a UseStateHandle<String>,
    pub(crate) hop_count: &'a UseStateHandle<String>,
    pub(crate) allow_short_paths: &'a UseStateHandle<bool>,
    pub(crate) headers: &'a UseStateHandle<String>,
    pub(crate) body: &'a UseStateHandle<String>,
    pub(crate) route_result: &'a UseStateHandle<String>,
    pub(crate) response_status: &'a UseStateHandle<String>,
    pub(crate) response_headers: &'a UseStateHandle<String>,
    pub(crate) response_body: &'a UseStateHandle<String>,
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

pub(crate) fn onion_proxy_panel(
    state: OnionProxyState<'_>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    status: UseStateHandle<String>,
) -> Html {
    let on_route = {
        let node_ref = node_ref.clone();
        let url = state.url.clone();
        let hop_count = state.hop_count.clone();
        let allow_short_paths = state.allow_short_paths.clone();
        let route_result = state.route_result.clone();
        let response_status = state.response_status.clone();
        let status = status.clone();
        Callback::from(move |_| {
            let options = match onion::OnionProxyOptions::from_input(
                hop_count.as_str(),
                *allow_short_paths,
            ) {
                Ok(options) => options,
                Err(error) => {
                    status.set(error);
                    return;
                }
            };
            let request = onion::OnionProxyRouteRequest {
                url: (*url).trim().to_string(),
                options,
            };
            let bridge = extension::extension_node_bridge();
            let node = node_ref.borrow().clone();
            let route_result = route_result.clone();
            let response_status = response_status.clone();
            let status = status.clone();
            status.set("building onion route".to_string());
            wasm_bindgen_futures::spawn_local(async move {
                let result = if let Some(bridge) = bridge {
                    extension::extension_onion_proxy_route(&bridge, request).await
                } else if let Some(node) = node {
                    onion::route(&node.provider, request).await
                } else {
                    Err("start the node first".to_string())
                };
                match result {
                    Ok(route) => {
                        let hop_count = route.hops.len();
                        route_result.set(route.summary());
                        response_status.set(format!("{hop_count} hops"));
                        status.set("onion route built".to_string());
                    }
                    Err(error) => status.set(error),
                }
            });
        })
    };
    let on_request = {
        let node_ref = node_ref.clone();
        let url = state.url.clone();
        let method = state.method.clone();
        let hop_count = state.hop_count.clone();
        let allow_short_paths = state.allow_short_paths.clone();
        let headers = state.headers.clone();
        let body = state.body.clone();
        let response_status = state.response_status.clone();
        let response_headers = state.response_headers.clone();
        let response_body = state.response_body.clone();
        let route_result = state.route_result.clone();
        let status = status.clone();
        Callback::from(move |_| {
            let options = match onion::OnionProxyOptions::from_input(
                hop_count.as_str(),
                *allow_short_paths,
            ) {
                Ok(options) => options,
                Err(error) => {
                    status.set(error);
                    return;
                }
            };
            let headers = match onion::parse_header_lines(headers.as_str()) {
                Ok(headers) => headers,
                Err(error) => {
                    status.set(error);
                    return;
                }
            };
            let request = onion::OnionProxyHttpRequest {
                url: (*url).trim().to_string(),
                method: (*method).trim().to_string(),
                headers,
                body: (*body).as_bytes().to_vec(),
                options,
            };
            let bridge = extension::extension_node_bridge();
            let node = node_ref.borrow().clone();
            let response_status = response_status.clone();
            let response_headers = response_headers.clone();
            let response_body = response_body.clone();
            let route_result = route_result.clone();
            let status = status.clone();
            status.set("sending onion proxy request".to_string());
            wasm_bindgen_futures::spawn_local(async move {
                let result = if let Some(bridge) = bridge {
                    extension::extension_onion_proxy_request(&bridge, request).await
                } else if let Some(node) = node {
                    onion::request(&node.provider, request).await
                } else {
                    Err("start the node first".to_string())
                };
                match result {
                    Ok(response) => {
                        response_status.set(response.status.to_string());
                        response_headers.set(onion::format_headers(&response.headers));
                        response_body.set(response.body);
                        route_result.set("request completed through onion HTTPS proxy".to_string());
                        status.set("onion proxy request completed".to_string());
                    }
                    Err(error) => status.set(error),
                }
            });
        })
    };

    html! {
        <section class="feature-panel" id="onion-proxy">
            <div class="section-heading">
                <p class="eyebrow">{ "Onion Proxy" }</p>
                <h2>{ "Route HTTPS requests through an onion exit" }</h2>
            </div>
            <div class="workflow-grid">
                <div class="tool-block">
                    <h3>{ "Request" }</h3>
                    { text_input("HTTPS URL", state.url.clone()) }
                    { text_input("Method", state.method.clone()) }
                    { text_input("Hop count", state.hop_count.clone()) }
                    { allow_short_paths_control(state.allow_short_paths.clone()) }
                    { textarea("Headers", state.headers.clone()) }
                    { textarea("Body", state.body.clone()) }
                    <div class="button-row">
                        <button onclick={on_route}>{ "Build route" }</button>
                        <button onclick={on_request}>{ "Send request" }</button>
                    </div>
                </div>
                <div class="tool-block">
                    <h3>{ "Result" }</h3>
                    <div class="proof-states">
                        { metric("HTTP", (**state.response_status).clone()) }
                    </div>
                    { readonly_output("Route", (**state.route_result).clone(), "No route built") }
                    { readonly_output("Response headers", (**state.response_headers).clone(), "No response headers") }
                    { readonly_output("Response body", (**state.response_body).clone(), "No response body") }
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

fn allow_short_paths_control(state: UseStateHandle<bool>) -> Html {
    let onchange = {
        let state = state.clone();
        Callback::from(move |event: Event| {
            if let Some(input) = event
                .target()
                .and_then(|target| target.dyn_into::<HtmlInputElement>().ok())
            {
                state.set(input.checked());
            }
        })
    };
    html! {
        <label class="field checkbox-field">
            <span>{ "Allow short paths" }</span>
            <input type="checkbox" checked={*state} {onchange} />
        </label>
    }
}

fn readonly_output(label: &'static str, value: String, placeholder: &'static str) -> Html {
    html! {
        <label class="field payload-output">
            <span>{ label }</span>
            <textarea readonly=true value={value} placeholder={placeholder} />
        </label>
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
