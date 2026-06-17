//! Rings dweb (Yew) — a self-contained decentralized-web demo.
//!
//! A Rust/Yew rewrite of the (deprecated, TypeScript) `rings-dweb`. Every node is both a
//! tiny static-site **host** and a **browser**: it serves pages to peers and fetches
//! pages from peers, peer-to-peer over rings — no central server. All of it runs over a
//! single `dweb` namespace registered with `provider.on(..)` (the JsProtocol path): the
//! handler answers requests with an `Effect::Send` and surfaces responses to the UI.
//!
//! Wire: a `dweb` message is JSON — `{"kind":"req","path":"/"}` or
//! `{"kind":"res","path":"/","body":"<html…>"}`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use js_sys::Array;
use js_sys::Function;
use js_sys::Object;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_node::prelude::rings_core::ecc::SecretKey;
use rings_node::prelude::rings_core::session::SessionSk;
use rings_node::prelude::rings_core::storage::idb::IdbStorage;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use serde::Deserialize;
use serde::Serialize;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_futures::JsFuture;
use web_sys::HtmlInputElement;
use yew::prelude::*;

/// A dweb request/response, carried as JSON in a `dweb` envelope.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
enum DwebMsg {
    /// Fetch `path` from the peer's hosted site.
    #[serde(rename = "req")]
    Req { path: String },
    /// The peer's answer for `path`.
    #[serde(rename = "res")]
    Res { path: String, body: String },
}

/// The site this node hosts: `path -> html`.
type Site = Rc<RefCell<HashMap<String, String>>>;

/// Build an in-browser node (IndexedDB), install the backend, start the message loop.
async fn build_node() -> Arc<Provider> {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).expect("session sk");
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk,
        200,
    );
    let storage = Box::new(
        IdbStorage::new_with_cap_and_name(50_000, "rings-dweb")
            .await
            .expect("idb storage"),
    );
    let processor = Arc::new(
        ProcessorBuilder::from_config(&config)
            .expect("processor builder")
            .storage(storage)
            .build()
            .expect("build processor"),
    );
    let provider = Arc::new(Provider::from_processor(processor));
    provider.set_backend().expect("install backend");

    let listening = provider.clone();
    spawn_local(async move {
        let _ = JsFuture::from(listening.listen()).await;
    });
    provider
}

/// The pure-shaped `dweb` handler `(ctx, event) -> { state, effects }`: a `req` is
/// answered with one `Send` effect carrying the hosted page; a `res` is pushed to the
/// UI. This is application code (the protocol engine stays untouched).
fn dweb_handle(ctx: &JsValue, event: &JsValue, site: &Site, on_response: &Callback<(String, String)>) -> JsValue {
    let result = Object::new();
    let state = Reflect::get(ctx, &"state".into()).unwrap_or(JsValue::NULL);
    let _ = Reflect::set(&result, &"state".into(), &state);
    let effects = Array::new();

    let from = Reflect::get(event, &"from".into())
        .ok()
        .and_then(|v| v.as_string());
    if let Ok(payload) = Reflect::get(event, &"payload".into()) {
        let bytes = Uint8Array::new(&payload).to_vec();
        if let Ok(msg) = serde_json::from_slice::<DwebMsg>(&bytes) {
            match msg {
                DwebMsg::Req { path } => {
                    if let Some(from) = from {
                        let body = site
                            .borrow()
                            .get(&path)
                            .cloned()
                            .unwrap_or_else(|| "<h1>404 not found</h1>".to_string());
                        if let Ok(out) = serde_json::to_vec(&DwebMsg::Res { path, body }) {
                            let effect = Object::new();
                            let _ = Reflect::set(&effect, &"to".into(), &JsValue::from_str(&from));
                            let _ = Reflect::set(&effect, &"namespace".into(), &"dweb".into());
                            let _ = Reflect::set(
                                &effect,
                                &"payload".into(),
                                &Uint8Array::from(out.as_slice()),
                            );
                            effects.push(&effect);
                        }
                    }
                }
                DwebMsg::Res { path, body } => on_response.emit((path, body)),
            }
        }
    }

    let _ = Reflect::set(&result, &"effects".into(), &effects);
    result.into()
}

/// Register the `dweb` protocol on the provider (serve + receive). The closure is leaked
/// (`forget`) to stay alive for the page's lifetime.
fn register_dweb(provider: &Arc<Provider>, site: Site, on_response: Callback<(String, String)>) {
    let handler = Closure::<dyn FnMut(JsValue, JsValue) -> JsValue>::new(
        move |ctx: JsValue, event: JsValue| dweb_handle(&ctx, &event, &site, &on_response),
    );
    let func: &Function = handler.as_ref().unchecked_ref();
    let _ = provider.on("dweb".to_string(), JsValue::NULL, func.clone());
    handler.forget();
}

/// Send a `req` for `path` to `peer` over the `dweb` namespace.
async fn fetch_path(provider: Arc<Provider>, peer: String, path: String) -> Result<(), String> {
    let bytes = serde_json::to_vec(&DwebMsg::Req { path }).map_err(|e| e.to_string())?;
    JsFuture::from(provider.send_message(peer, "dweb".to_string(), Uint8Array::from(bytes.as_slice())))
        .await
        .map(|_| ())
        .map_err(|e| format!("send failed: {e:?}"))
}

fn input_value(e: &InputEvent) -> String {
    e.target_unchecked_into::<HtmlInputElement>().value()
}

#[function_component(App)]
fn app() -> Html {
    let provider: Rc<RefCell<Option<Arc<Provider>>>> = use_mut_ref(|| None);
    let did = use_state(String::new);
    let status = use_state(|| "starting node…".to_string());
    let peer_did = use_state(String::new);
    let path = use_state(|| "/".to_string());
    let page = use_state(String::new);

    {
        let provider = provider.clone();
        let did = did.clone();
        let status = status.clone();
        let page = page.clone();
        use_effect_with((), move |_| {
            spawn_local(async move {
                let p = build_node().await;
                let my_did = p.address();
                did.set(my_did.clone());

                let mut site = HashMap::new();
                site.insert(
                    "/".to_string(),
                    format!("<h1>Hello from {my_did}</h1><p>Served peer-to-peer over rings dweb.</p>"),
                );
                let site: Site = Rc::new(RefCell::new(site));

                let on_response = {
                    let page = page.clone();
                    Callback::from(move |(path, body): (String, String)| {
                        page.set(format!("<!-- {path} -->\n{body}"))
                    })
                };
                register_dweb(&p, site, on_response);

                *provider.borrow_mut() = Some(p);
                status.set("ready — paste a peer DID and fetch a path".to_string());
            });
            || ()
        });
    }

    let on_peer = {
        let peer_did = peer_did.clone();
        Callback::from(move |e: InputEvent| peer_did.set(input_value(&e)))
    };
    let on_path = {
        let path = path.clone();
        Callback::from(move |e: InputEvent| path.set(input_value(&e)))
    };

    let on_fetch = {
        let provider = provider.clone();
        let status = status.clone();
        let peer_did = peer_did.clone();
        let path = path.clone();
        Callback::from(move |_| {
            let Some(p) = provider.borrow().clone() else {
                return;
            };
            let (peer, path) = ((*peer_did).trim().to_string(), (*path).clone());
            if peer.is_empty() {
                status.set("enter a peer DID".to_string());
                return;
            }
            let status = status.clone();
            status.set(format!("fetching {path} from {peer}…"));
            spawn_local(async move {
                match fetch_path(p, peer, path).await {
                    Ok(()) => status.set("request sent — waiting for response".to_string()),
                    Err(e) => status.set(e),
                }
            });
        })
    };

    let page_html = Html::from_html_unchecked(AttrValue::from((*page).clone()));

    html! {
        <main style="font-family: system-ui; max-width: 720px; margin: 2rem auto;">
            <h1>{ "Rings dweb" }</h1>
            <p><b>{ "this node: " }</b><code>{ (*did).clone() }</code>
               { " — it hosts " }<code>{ "/" }</code></p>
            <fieldset>
                <legend>{ "fetch a page from a peer" }</legend>
                <p><input placeholder="peer DID (0x…)" value={(*peer_did).clone()} oninput={on_peer} size="52" /></p>
                <p><input value={(*path).clone()} oninput={on_path} size="20" />
                   <button onclick={on_fetch}>{ "fetch" }</button></p>
            </fieldset>
            <p><b>{ "status: " }</b>{ (*status).clone() }</p>
            <hr/>
            <div>{ page_html }</div>
        </main>
    }
}

/// Mount the Yew app.
pub fn run() {
    yew::Renderer::<App>::new().render();
}
