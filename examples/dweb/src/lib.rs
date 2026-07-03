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

use futures::future::AbortHandle;
use futures::future::Abortable;
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

struct DwebNode {
    provider: Arc<Provider>,
    listen_abort: AbortHandle,
}

impl DwebNode {
    fn stop(&self) {
        self.listen_abort.abort();
    }
}

/// Build an in-browser node (IndexedDB under `storage_name`), install the backend, start
/// the message loop. Distinct `storage_name`s let two nodes coexist (e.g. in a test).
///
/// The browser provider is used only on the single-threaded wasm event loop, but
/// the upstream `Provider` constructor takes an `Arc<Processor>`; keep that shape
/// at this adapter boundary instead of introducing a parallel wasm-only provider.
#[allow(clippy::arc_with_non_send_sync)]
async fn build_node(storage_name: &str) -> DwebNode {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).expect("session sk");
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk,
        200,
    );
    let storage = Box::new(
        IdbStorage::new_with_cap_and_name(50_000, storage_name)
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
    let listening = processor.clone();
    let provider = Arc::new(Provider::from_processor(processor));
    provider.set_backend().expect("install backend");

    let (listen_abort, listen_registration) = AbortHandle::new_pair();
    spawn_local(async move {
        let _ = Abortable::new(listening.listen(), listen_registration).await;
    });

    DwebNode {
        provider,
        listen_abort,
    }
}

/// The pure-shaped `dweb` handler `(ctx, event) -> { state, effects }`: a `req` is
/// answered with one `Send` effect carrying the hosted page; a `res` is pushed to the
/// UI. This is application code (the protocol engine stays untouched).
fn dweb_handle(
    ctx: &JsValue,
    event: &JsValue,
    site: &Site,
    on_response: &Callback<(String, String)>,
) -> JsValue {
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
                            // Effects are namespace-scoped: the send goes out under this
                            // protocol's own namespace, so an effect carries only `to`+`payload`.
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
    JsFuture::from(provider.send_message(
        peer,
        "dweb".to_string(),
        Uint8Array::from(bytes.as_slice()),
    ))
    .await
    .map(|_| ())
    .map_err(|e| format!("send failed: {e:?}"))
}

fn input_value(e: &InputEvent) -> String {
    e.target_unchecked_into::<HtmlInputElement>().value()
}

#[function_component(App)]
fn app() -> Html {
    let node: Rc<RefCell<Option<DwebNode>>> = use_mut_ref(|| None);
    let did = use_state(String::new);
    let status = use_state(|| "starting node…".to_string());
    let peer_did = use_state(String::new);
    let path = use_state(|| "/".to_string());
    let page = use_state(String::new);

    {
        let node = node.clone();
        let did = did.clone();
        let status = status.clone();
        let page = page.clone();
        use_effect_with((), move |_| {
            let node_for_task = node.clone();
            spawn_local(async move {
                let built = build_node("rings-dweb").await;
                let p = built.provider.clone();
                let my_did = p.address();
                did.set(my_did.clone());

                let mut site = HashMap::new();
                site.insert(
                    "/".to_string(),
                    format!(
                        "<h1>Hello from {my_did}</h1><p>Served peer-to-peer over rings dweb.</p>"
                    ),
                );
                let site: Site = Rc::new(RefCell::new(site));

                let on_response = {
                    let page = page.clone();
                    Callback::from(move |(path, body): (String, String)| {
                        page.set(format!("<!-- {path} -->\n{body}"))
                    })
                };
                register_dweb(&p, site, on_response);

                *node_for_task.borrow_mut() = Some(built);
                status.set("ready — paste a peer DID and fetch a path".to_string());
            });
            move || {
                if let Some(node) = node.borrow_mut().take() {
                    node.stop();
                }
            }
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
        let node = node.clone();
        let status = status.clone();
        let peer_did = peer_did.clone();
        let path = path.clone();
        Callback::from(move |_| {
            let Some(p) = node
                .borrow()
                .as_ref()
                .map(|node| node.provider.clone())
            else {
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
            // Peer-controlled HTML is rendered inside a maximally-constrained iframe:
            // `srcdoc` carries the body and the empty `sandbox` attribute applies all
            // restrictions (scripts disabled, opaque origin, no forms/popups), so a hostile
            // page cannot run JS or reach the app's DOM/origin. This is the security model.
            <iframe
                title="peer page"
                srcdoc={(*page).clone()}
                sandbox=""
                style="width: 100%; min-height: 320px; border: 1px solid #ccc;"
            />
        </main>
    }
}

/// Mount the Yew app.
pub fn run() {
    yew::Renderer::<App>::new().render();
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use wasm_bindgen_test::wasm_bindgen_test;
    use wasm_bindgen_test::wasm_bindgen_test_configure;

    use super::*;

    // Run in a real (headless) browser for parity with proof-demo:
    // `wasm-pack test --headless --chrome` (needs a chromedriver matching your Chrome;
    // `webdriver.json` supplies the launch flags). The logic itself is browser-free.
    wasm_bindgen_test_configure!(run_in_browser);

    fn ctx_null() -> JsValue {
        let ctx = Object::new();
        Reflect::set(&ctx, &"state".into(), &JsValue::NULL).unwrap();
        ctx.into()
    }

    fn event(from: &str, msg: &DwebMsg) -> JsValue {
        let event = Object::new();
        Reflect::set(&event, &"from".into(), &JsValue::from_str(from)).unwrap();
        let bytes = serde_json::to_vec(msg).unwrap();
        Reflect::set(
            &event,
            &"payload".into(),
            &Uint8Array::from(bytes.as_slice()),
        )
        .unwrap();
        event.into()
    }

    fn effects(result: &JsValue) -> Array {
        Array::from(&Reflect::get(result, &"effects".into()).unwrap())
    }

    fn effect_response(effect: &JsValue) -> DwebMsg {
        let payload = Reflect::get(effect, &"payload".into()).unwrap();
        serde_json::from_slice(&Uint8Array::new(&payload).to_vec()).unwrap()
    }

    #[wasm_bindgen_test]
    fn serves_a_hosted_page_back_to_the_requester() {
        let site: Site = Rc::new(RefCell::new(HashMap::from([(
            "/".to_string(),
            "<h1>hi</h1>".to_string(),
        )])));
        let result = dweb_handle(
            &ctx_null(),
            &event("0xabc", &DwebMsg::Req { path: "/".into() }),
            &site,
            &Callback::from(|_| {}),
        );
        let effects = effects(&result);
        assert_eq!(effects.length(), 1, "a request yields exactly one Send");
        let effect = effects.get(0);
        assert_eq!(
            Reflect::get(&effect, &"to".into())
                .unwrap()
                .as_string()
                .unwrap(),
            "0xabc"
        );
        match effect_response(&effect) {
            DwebMsg::Res { path, body } => {
                assert_eq!(path, "/");
                assert_eq!(body, "<h1>hi</h1>");
            }
            _ => panic!("expected a Res"),
        }
    }

    #[wasm_bindgen_test]
    fn unknown_path_yields_404() {
        let site: Site = Rc::new(RefCell::new(HashMap::new()));
        let result = dweb_handle(
            &ctx_null(),
            &event("0xabc", &DwebMsg::Req {
                path: "/missing".into(),
            }),
            &site,
            &Callback::from(|_| {}),
        );
        match effect_response(&effects(&result).get(0)) {
            DwebMsg::Res { body, .. } => assert!(body.contains("404")),
            _ => panic!("expected a Res"),
        }
    }

    #[wasm_bindgen_test]
    fn a_response_is_surfaced_and_sends_nothing() {
        let site: Site = Rc::new(RefCell::new(HashMap::new()));
        let got: Rc<RefCell<Option<(String, String)>>> = Rc::new(RefCell::new(None));
        let cb = {
            let got = got.clone();
            Callback::from(move |r| *got.borrow_mut() = Some(r))
        };
        let result = dweb_handle(
            &ctx_null(),
            &event("0xabc", &DwebMsg::Res {
                path: "/".into(),
                body: "PAGE".into(),
            }),
            &site,
            &cb,
        );
        assert_eq!(effects(&result).length(), 0, "a response triggers no Send");
        assert_eq!(*got.borrow(), Some(("/".to_string(), "PAGE".to_string())));
    }

    // ── End-to-end: two real nodes, one fetches a page hosted by the other ──────────

    /// A JS object of `{key: value}` string fields, for `provider.request` params.
    fn obj(pairs: &[(&str, &str)]) -> JsValue {
        let o = Object::new();
        for (k, v) in pairs {
            Reflect::set(&o, &(*k).into(), &JsValue::from_str(v)).unwrap();
        }
        o.into()
    }

    fn get_str(value: &JsValue, key: &str) -> String {
        Reflect::get(value, &key.into())
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| panic!("missing string field {key:?}"))
    }

    async fn rpc(provider: &Arc<Provider>, method: &str, params: JsValue) -> JsValue {
        JsFuture::from(provider.request(method.to_string(), params))
            .await
            .unwrap_or_else(|e| panic!("rpc {method} failed: {e:?}"))
    }

    /// Link two in-page providers with the offer/answer handshake (no signaling server).
    async fn connect(a: &Arc<Provider>, b: &Arc<Provider>) {
        let offer = get_str(
            &rpc(a, "createOffer", obj(&[("did", &b.address())])).await,
            "offer",
        );
        let answer = get_str(
            &rpc(b, "answerOffer", obj(&[("offer", &offer)])).await,
            "answer",
        );
        let _ = rpc(a, "acceptAnswer", obj(&[("answer", &answer)])).await;
    }

    /// Two nodes: B hosts `/`, A connects and fetches it over rings, expecting B's page.
    #[wasm_bindgen_test]
    async fn two_nodes_fetch_a_hosted_page() {
        use rings_node::prelude::rings_core::utils::js_utils::window_sleep;

        // B hosts a page.
        let b = build_node("rings-dweb-test-b").await;
        register_dweb(
            &b.provider,
            Rc::new(RefCell::new(HashMap::from([(
                "/".to_string(),
                "<h1>from B</h1>".to_string(),
            )]))),
            Callback::from(|_| {}),
        );

        // A is the fetcher; it records the page it receives.
        let got: Rc<RefCell<Option<(String, String)>>> = Rc::new(RefCell::new(None));
        let a = build_node("rings-dweb-test-a").await;
        register_dweb(&a.provider, Rc::new(RefCell::new(HashMap::new())), {
            let got = got.clone();
            Callback::from(move |r| *got.borrow_mut() = Some(r))
        });

        connect(&a.provider, &b.provider).await;

        // Retry the request until the overlay link is up and B's response arrives.
        let b_did = b.provider.address();
        for _ in 0..60 {
            let _ = fetch_path(a.provider.clone(), b_did.clone(), "/".to_string()).await;
            window_sleep(500).await.ok();
            if got.borrow().is_some() {
                break;
            }
        }

        let page = got.borrow().clone().expect("no response received from B");
        assert_eq!(page.0, "/");
        assert_eq!(page.1, "<h1>from B</h1>");
        a.stop();
        b.stop();
    }
}
