//! Workbench panels for onion proxy, proof, and custom messages.

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use futures::future::Either;
use futures::FutureExt;
use gloo_timers::future::sleep;
use rings_node::extension::snark::ProofResult;
use wasm_bindgen::JsCast;
use web_sys::Event;
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::controls::metric;
use crate::custom;
use crate::extension;
use crate::forms::text_input;
use crate::forms::textarea;
use crate::generation::GenerationClock;
use crate::generation::GenerationToken;
use crate::node::DemoNode;
use crate::onion;
use crate::proof;

const ONION_ROUTE_TIMEOUT: Duration = Duration::from_secs(20);
const ONION_REQUEST_TIMEOUT: Duration = Duration::from_secs(35);

struct OnionProxyBackend {
    bridge: Option<wasm_bindgen::JsValue>,
    node: Option<DemoNode>,
    token: GenerationToken,
}

struct OnionRequestOutputs {
    response_status: UseStateHandle<String>,
    response_headers: UseStateHandle<String>,
    response_body: UseStateHandle<String>,
    route_result: UseStateHandle<String>,
    status: UseStateHandle<String>,
    token: GenerationToken,
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

pub(crate) fn onion_proxy_panel(
    state: OnionProxyState<'_>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    status: UseStateHandle<String>,
) -> Html {
    let on_route =
        onion_route_callback(&state, node_ref.clone(), generation.clone(), status.clone());
    let on_request = onion_request_callback(&state, node_ref, generation, status);

    html! {
        <section class="feature-panel" id="onion-proxy">
            <div class="section-heading">
                <p class="eyebrow">{ "Onion Proxy" }</p>
                <h2>{ "Route HTTPS requests through an onion exit" }</h2>
            </div>
            <div class="workflow-grid">
                { onion_request_form(&state, on_route, on_request) }
                { onion_result_panel(&state) }
            </div>
        </section>
    }
}

fn onion_route_callback(
    state: &OnionProxyState<'_>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    status: UseStateHandle<String>,
) -> Callback<MouseEvent> {
    let url = state.url.clone();
    let hop_count = state.hop_count.clone();
    let allow_short_paths = state.allow_short_paths.clone();
    let route_result = state.route_result.clone();
    let response_status = state.response_status.clone();
    Callback::from(move |_| {
        let options =
            match onion::OnionProxyOptions::from_input(hop_count.as_str(), *allow_short_paths) {
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
        let backend = OnionProxyBackend::current(&node_ref, &generation);
        let token = backend.token.clone();
        let route_result = route_result.clone();
        let response_status = response_status.clone();
        let status = status.clone();
        status.set("building onion route".to_string());
        route_result.set("Building route...".to_string());
        response_status.set("routing".to_string());
        wasm_bindgen_futures::spawn_local(async move {
            let result = build_onion_route(backend, request).await;
            if !token.is_current() {
                return;
            }
            match result {
                Ok(route) => {
                    let hop_count = route.hops.len();
                    route_result.set(route.summary());
                    response_status.set(format!("{hop_count} hops"));
                    status.set("onion route built".to_string());
                }
                Err(error) => {
                    route_result.set(error.clone());
                    response_status.set("failed".to_string());
                    status.set(error);
                }
            }
        });
    })
}

async fn build_onion_route(
    backend: OnionProxyBackend,
    request: onion::OnionProxyRouteRequest,
) -> Result<onion::OnionProxyRoute, String> {
    operation_timeout("onion route build", ONION_ROUTE_TIMEOUT, async move {
        if let Some(bridge) = backend.bridge {
            extension::extension_onion_proxy_route(&bridge, request).await
        } else if let Some(node) = backend.node {
            onion::route(&node.provider, request)
                .await
                .map_err(|error| error.to_string())
        } else {
            Err("start the node first".to_string())
        }
    })
    .await
}

fn onion_request_callback(
    state: &OnionProxyState<'_>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    generation: GenerationClock,
    status: UseStateHandle<String>,
) -> Callback<MouseEvent> {
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
    Callback::from(move |_| {
        let request = match onion_http_request(
            &url,
            &method,
            &headers,
            &body,
            &hop_count,
            *allow_short_paths,
        ) {
            Ok(request) => request,
            Err(error) => {
                status.set(error);
                return;
            }
        };
        let backend = OnionProxyBackend::current(&node_ref, &generation);
        let token = backend.token.clone();
        prepare_onion_request_outputs(
            &status,
            &response_status,
            &response_headers,
            &response_body,
            &route_result,
        );
        spawn_onion_request(backend, request, OnionRequestOutputs {
            response_status: response_status.clone(),
            response_headers: response_headers.clone(),
            response_body: response_body.clone(),
            route_result: route_result.clone(),
            status: status.clone(),
            token,
        });
    })
}

impl OnionProxyBackend {
    fn current(node_ref: &Rc<RefCell<Option<DemoNode>>>, generation: &GenerationClock) -> Self {
        Self {
            bridge: extension::extension_node_bridge(),
            node: node_ref.borrow().clone(),
            token: generation.token(),
        }
    }
}

fn onion_http_request(
    url: &UseStateHandle<String>,
    method: &UseStateHandle<String>,
    headers: &UseStateHandle<String>,
    body: &UseStateHandle<String>,
    hop_count: &UseStateHandle<String>,
    allow_short_paths: bool,
) -> Result<onion::OnionProxyHttpRequest, String> {
    let options = onion::OnionProxyOptions::from_input(hop_count.as_str(), allow_short_paths)?;
    let headers = onion::parse_header_lines(headers.as_str())?;
    Ok(onion::OnionProxyHttpRequest {
        url: (*url).trim().to_string(),
        method: (*method).trim().to_string(),
        headers,
        body: (*body).as_bytes().to_vec(),
        options,
    })
}

fn prepare_onion_request_outputs(
    status: &UseStateHandle<String>,
    response_status: &UseStateHandle<String>,
    response_headers: &UseStateHandle<String>,
    response_body: &UseStateHandle<String>,
    route_result: &UseStateHandle<String>,
) {
    status.set("sending onion proxy request".to_string());
    response_status.set("sending".to_string());
    response_headers.set(String::new());
    response_body.set(String::new());
    route_result.set("Sending request through onion HTTPS proxy...".to_string());
}

fn spawn_onion_request(
    backend: OnionProxyBackend,
    request: onion::OnionProxyHttpRequest,
    outputs: OnionRequestOutputs,
) {
    wasm_bindgen_futures::spawn_local(async move {
        let result = send_onion_request(backend, request).await;
        if !outputs.token.is_current() {
            return;
        }
        match result {
            Ok(response) => {
                outputs.response_status.set(response.status.to_string());
                outputs
                    .response_headers
                    .set(onion::format_headers(&response.headers));
                outputs.response_body.set(response.body_text());
                outputs
                    .route_result
                    .set("request completed through onion HTTPS proxy".to_string());
                outputs
                    .status
                    .set("onion proxy request completed".to_string());
            }
            Err(error) => {
                outputs.response_status.set("failed".to_string());
                outputs.response_body.set(error.clone());
                outputs.status.set(error);
            }
        }
    });
}

async fn send_onion_request(
    backend: OnionProxyBackend,
    request: onion::OnionProxyHttpRequest,
) -> Result<onion::OnionProxyResponse, String> {
    operation_timeout("onion proxy request", ONION_REQUEST_TIMEOUT, async move {
        if let Some(bridge) = backend.bridge {
            extension::extension_onion_proxy_request(&bridge, request).await
        } else if let Some(node) = backend.node {
            onion::request(&node.provider, request)
                .await
                .map_err(|error| error.to_string())
        } else {
            Err("start the node first".to_string())
        }
    })
    .await
}

fn onion_request_form(
    state: &OnionProxyState<'_>,
    on_route: Callback<MouseEvent>,
    on_request: Callback<MouseEvent>,
) -> Html {
    html! {
        <div class="tool-block">
            <h3>{ "Request" }</h3>
            { text_input("HTTPS URL", state.url.clone()) }
            { text_input("Method", state.method.clone()) }
            { text_input("Hop count", state.hop_count.clone()) }
            { allow_short_paths_control(state.allow_short_paths.clone()) }
            { textarea("Headers", state.headers.clone()) }
            { textarea("Body", state.body.clone()) }
            <div class="button-row">
                <button type="button" onclick={on_route}>{ "Build route" }</button>
                <button type="button" onclick={on_request}>{ "Send request" }</button>
            </div>
        </div>
    }
}

fn onion_result_panel(state: &OnionProxyState<'_>) -> Html {
    html! {
        <div class="tool-block">
            <h3>{ "Result" }</h3>
            <div class="proof-states">
                { metric("HTTP", (**state.response_status).clone()) }
            </div>
            { readonly_output("Route", (**state.route_result).clone(), "No route built") }
            { readonly_output("Response headers", (**state.response_headers).clone(), "No response headers") }
            { readonly_output("Response body", (**state.response_body).clone(), "No response body") }
        </div>
    }
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
    match futures::future::select(operation, timer).await {
        Either::Left((result, _)) => result,
        Either::Right((_, _)) => Err(format!("{label} timed out")),
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
    let on_register = custom_register_callback(
        namespace,
        registered,
        events,
        node_ref.clone(),
        status.clone(),
    );
    let on_send = custom_send_callback(namespace, peer, payload, node_ref, status);
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
                { custom_events_panel(events) }
            </div>
        </section>
    }
}

fn custom_register_callback(
    namespace: &UseStateHandle<String>,
    registered: &UseStateHandle<Vec<String>>,
    events: &UseStateHandle<Vec<custom::CustomEvent>>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    status: UseStateHandle<String>,
) -> Callback<MouseEvent> {
    let namespace = namespace.clone();
    let registered = registered.clone();
    let events = events.clone();
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
        let on_custom = custom_event_callback(events.clone());
        match custom::register(&node.provider, ns.clone(), on_custom) {
            Ok(()) => record_registered_namespace(&registered, &status, ns),
            Err(error) => status.set(error),
        }
    })
}

fn custom_event_callback(
    events: UseStateHandle<Vec<custom::CustomEvent>>,
) -> Callback<custom::CustomEvent> {
    Callback::from(move |event: custom::CustomEvent| {
        let mut next = (*events).clone();
        next.insert(0, event);
        next.truncate(20);
        events.set(next);
    })
}

fn record_registered_namespace(
    registered: &UseStateHandle<Vec<String>>,
    status: &UseStateHandle<String>,
    namespace: String,
) {
    let mut next = (**registered).clone();
    next.push(namespace.clone());
    registered.set(next);
    status.set(format!("registered {namespace}"));
}

fn custom_send_callback(
    namespace: &UseStateHandle<String>,
    peer: &UseStateHandle<String>,
    payload: &UseStateHandle<String>,
    node_ref: Rc<RefCell<Option<DemoNode>>>,
    status: UseStateHandle<String>,
) -> Callback<MouseEvent> {
    let namespace = namespace.clone();
    let peer = peer.clone();
    let payload = payload.clone();
    Callback::from(move |_| {
        let Some(node) = node_ref.borrow().clone() else {
            status.set("start the node first".to_string());
            return;
        };
        let Some(message) = custom_message(&namespace, &peer, &payload, &status) else {
            return;
        };
        let status = status.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match custom::send(
                node.provider.clone(),
                message.did,
                message.namespace,
                message.payload,
            )
            .await
            {
                Ok(()) => status.set("custom message sent".to_string()),
                Err(error) => status.set(error),
            }
        });
    })
}

struct CustomMessage {
    namespace: String,
    did: String,
    payload: String,
}

fn custom_message(
    namespace: &UseStateHandle<String>,
    peer: &UseStateHandle<String>,
    payload: &UseStateHandle<String>,
    status: &UseStateHandle<String>,
) -> Option<CustomMessage> {
    let namespace = (*namespace).trim().to_string();
    let did = (*peer).trim().to_string();
    if namespace.is_empty() || did.is_empty() {
        status.set("enter namespace and destination DID".to_string());
        return None;
    }
    Some(CustomMessage {
        namespace,
        did,
        payload: (**payload).clone(),
    })
}

fn custom_events_panel(events: &UseStateHandle<Vec<custom::CustomEvent>>) -> Html {
    html! {
        <div class="tool-block">
            <h3>{ "Inbound" }</h3>
            <div class="list">
                { for events.iter().map(custom_event_item) }
            </div>
        </div>
    }
}

fn custom_event_item(event: &custom::CustomEvent) -> Html {
    html! {
        <div class="list-item">
            <div>
                <b>{ event.namespace.clone() }</b>
                { " from " }
                <span class="mono">{ event.from.clone() }</span>
            </div>
            <div>{ event.payload.clone() }</div>
        </div>
    }
}
