//! Rings proof-demo (Yew) — distributed SNARK over the rings overlay.
//!
//! A Rust/Yew rewrite of the (deprecated, TypeScript) `rings-proof-demo`. This node is
//! the **verifier**: it builds a recursive SNARK proof task from a circuit, offloads the
//! heavy proving to a **prover** peer over rings, and verifies the returned proof — all
//! through the same `SnarkProtocol` the daemon uses (`gen_and_send_proof_task` →
//! `Effect::Compute` on the prover → reply → `get_task_result`).
//!
//! The rings wiring is kept in `rings`-prefixed helpers; the rest is a thin Yew UI.

use std::cell::RefCell;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use futures::future::AbortHandle;
use futures::future::Abortable;
use gloo_timers::future::sleep;
use rings_node::extension::snark::Field;
use rings_node::extension::snark::Input;
use rings_node::extension::snark::ProofResult;
use rings_node::extension::snark::SNARKBehaviour;
use rings_node::extension::snark::SNARKTaskBuilder;
use rings_node::extension::snark::SupportedPrimeField;
use rings_node::prelude::rings_core::dht::Did;
use rings_node::prelude::rings_core::ecc::SecretKey;
use rings_node::prelude::rings_core::session::SessionSk;
use rings_node::prelude::rings_core::storage::idb::IdbStorage;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_futures::JsFuture;
use web_sys::HtmlInputElement;
use yew::prelude::*;

/// A ready node: the provider plus the SNARK behaviour sharing its task store.
#[derive(Clone)]
struct Node {
    provider: Arc<Provider>,
    snark: SNARKBehaviour,
    listen_abort: AbortHandle,
}

impl Node {
    fn stop(&self) {
        self.listen_abort.abort();
    }
}

/// Build an in-browser node (IndexedDB storage), install the extension backend, register
/// the SNARK protocol, and start the message loop. Mirrors how the daemon is wired.
///
/// The browser provider is used only on the single-threaded wasm event loop, but
/// the upstream `Provider` constructor takes an `Arc<Processor>`; keep that shape
/// at this adapter boundary instead of introducing a parallel wasm-only provider.
#[allow(clippy::arc_with_non_send_sync)]
async fn build_node_with_storage(storage_name: &str) -> Result<Node, String> {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).map_err(|e| format!("session sk: {e}"))?;
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk,
        200,
    );
    let storage = Box::new(
        IdbStorage::new_with_cap_and_name(50_000, storage_name)
            .await
            .map_err(|e| format!("idb storage: {e}"))?,
    );
    let processor = Arc::new(
        ProcessorBuilder::from_config(&config)
            .map_err(|e| format!("processor builder: {e}"))?
            .storage(storage)
            .build()
            .map_err(|e| format!("build processor: {e}"))?,
    );
    let listening = processor.clone();
    let provider = Arc::new(Provider::from_processor(processor));
    provider
        .set_backend()
        .map_err(|e| format!("install backend: {e}"))?;
    let snark = SNARKBehaviour::default();
    snark
        .register(&provider)
        .map_err(|e| format!("register snark: {e}"))?;

    let (listen_abort, listen_registration) = AbortHandle::new_pair();
    spawn_local(async move {
        let _ = Abortable::new(listening.listen(), listen_registration).await;
    });

    Ok(Node {
        provider,
        snark,
        listen_abort,
    })
}

/// Build the UI node with its stable IndexedDB name.
async fn build_node() -> Result<Node, String> {
    build_node_with_storage("rings-proof-demo").await
}

/// Join the overlay via a seed node's HTTP endpoint.
async fn connect_seed(provider: &Arc<Provider>, seed_url: String) -> Result<(), String> {
    JsFuture::from(provider.connect_peer_via_http(seed_url))
        .await
        .map(|_| ())
        .map_err(|e| format!("connect failed: {e:?}"))
}

/// The public input for the bundled `simple_bn256` circuit: `step_in = [4, 2]` (Vesta).
fn sample_input() -> Input {
    vec![("step_in".to_string(), vec![
        Field::from_u64(4, SupportedPrimeField::Vesta),
        Field::from_u64(2, SupportedPrimeField::Vesta),
    ])]
    .into()
}

/// Offload a proof to `prover` and wait for its result.
///
/// Loads the circuit from `r1cs_url`/`wasm_url`, generates a small recursive proof task
/// (sample input `step_in = [4, 2]`, 5 rounds, Vesta), sends it to the prover, and polls
/// the local task store. Returns as soon as a result arrives ([`ProofResult::Verified`] or
/// [`ProofResult::Invalid`]); if none arrives within the window it returns
/// [`ProofResult::Pending`] (a timeout), which the caller reports distinctly from an
/// invalid proof.
async fn run_proof(
    node: Node,
    prover: Did,
    r1cs_url: String,
    wasm_url: String,
) -> Result<ProofResult, String> {
    let builder = SNARKTaskBuilder::from_remote(r1cs_url, wasm_url, SupportedPrimeField::Vesta)
        .await
        .map_err(|e| format!("load circuit failed: {e}"))?;

    let circuits = builder
        .gen_circuits(sample_input(), vec![], 5)
        .map_err(|e| format!("gen circuits failed: {e}"))?;

    let task_id = node
        .snark
        .gen_and_send_proof_task(node.provider.clone(), circuits, prover)
        .await
        .map_err(|e| format!("send proof task failed: {e}"))?;

    for _ in 0..60 {
        sleep(Duration::from_secs(1)).await;
        let result = node
            .snark
            .get_task_result(task_id.clone())
            .map_err(|e| format!("read result failed: {e}"))?;
        if result != ProofResult::Pending {
            return Ok(result);
        }
    }
    Ok(ProofResult::Pending)
}

/// Read the current value of an `<input>` from an input event.
fn input_value(e: &InputEvent) -> String {
    e.target_unchecked_into::<HtmlInputElement>().value()
}

#[function_component(App)]
fn app() -> Html {
    let node: Rc<RefCell<Option<Node>>> = use_mut_ref(|| None);
    let did = use_state(String::new);
    let status = use_state(|| "starting node…".to_string());
    let seed_url = use_state(|| "http://127.0.0.1:50000".to_string());
    let prover_did = use_state(String::new);
    let r1cs_url = use_state(|| "http://127.0.0.1:8080/simple_bn256.r1cs".to_string());
    let wasm_url = use_state(|| "http://127.0.0.1:8080/simple_bn256.wasm".to_string());

    {
        let node = node.clone();
        let did = did.clone();
        let status = status.clone();
        use_effect_with((), move |_| {
            let node_for_task = node.clone();
            spawn_local(async move {
                match build_node().await {
                    Ok(built) => {
                        did.set(built.provider.address());
                        if let Some(previous) = node_for_task.borrow_mut().replace(built) {
                            previous.stop();
                        }
                        status.set("ready — connect to a seed, then send a proof".to_string());
                    }
                    Err(e) => status.set(format!("node init failed: {e}")),
                }
            });
            move || {
                if let Some(node) = node.borrow_mut().take() {
                    node.stop();
                }
            }
        });
    }

    let on_seed = {
        let seed_url = seed_url.clone();
        Callback::from(move |e: InputEvent| seed_url.set(input_value(&e)))
    };
    let on_prover = {
        let prover_did = prover_did.clone();
        Callback::from(move |e: InputEvent| prover_did.set(input_value(&e)))
    };
    let on_r1cs = {
        let r1cs_url = r1cs_url.clone();
        Callback::from(move |e: InputEvent| r1cs_url.set(input_value(&e)))
    };
    let on_wasm = {
        let wasm_url = wasm_url.clone();
        Callback::from(move |e: InputEvent| wasm_url.set(input_value(&e)))
    };

    let on_connect = {
        let node = node.clone();
        let status = status.clone();
        let seed_url = seed_url.clone();
        Callback::from(move |_| {
            let Some(n) = node.borrow().clone() else {
                return;
            };
            let status = status.clone();
            let url = (*seed_url).clone();
            status.set(format!("connecting to {url}…"));
            spawn_local(async move {
                match connect_seed(&n.provider, url).await {
                    Ok(()) => status.set("connected to seed".to_string()),
                    Err(e) => status.set(e),
                }
            });
        })
    };

    let on_prove = {
        let node = node.clone();
        let status = status.clone();
        let prover_did = prover_did.clone();
        let r1cs_url = r1cs_url.clone();
        let wasm_url = wasm_url.clone();
        Callback::from(move |_| {
            let Some(n) = node.borrow().clone() else {
                return;
            };
            let prover = match Did::from_str(prover_did.trim()) {
                Ok(did) => did,
                Err(_) => {
                    status.set("invalid prover DID".to_string());
                    return;
                }
            };
            let status = status.clone();
            let (r1cs, wasm) = ((*r1cs_url).clone(), (*wasm_url).clone());
            status.set("offloading proof to prover…".to_string());
            spawn_local(async move {
                match run_proof(n, prover, r1cs, wasm).await {
                    Ok(ProofResult::Verified) => status.set("✅ proof verified".to_string()),
                    Ok(ProofResult::Invalid) => {
                        status.set("❌ proof returned but failed verification".to_string())
                    }
                    Ok(ProofResult::Pending) => {
                        status.set("⌛ timed out waiting for proof".to_string())
                    }
                    Err(e) => status.set(format!("❌ {e}")),
                }
            });
        })
    };

    html! {
        <main style="font-family: system-ui; max-width: 640px; margin: 2rem auto;">
            <h1>{ "Rings proof-demo — distributed SNARK" }</h1>
            <p><b>{ "this node (verifier): " }</b><code>{ (*did).clone() }</code></p>
            <fieldset>
                <legend>{ "1. join the overlay" }</legend>
                <input value={(*seed_url).clone()} oninput={on_seed} size="48" />
                <button onclick={on_connect}>{ "connect to seed" }</button>
            </fieldset>
            <fieldset>
                <legend>{ "2. offload a proof to a prover peer" }</legend>
                <p><input placeholder="prover DID (0x…)" value={(*prover_did).clone()} oninput={on_prover} size="48" /></p>
                <p><input value={(*r1cs_url).clone()} oninput={on_r1cs} size="48" /></p>
                <p><input value={(*wasm_url).clone()} oninput={on_wasm} size="48" /></p>
                <button onclick={on_prove}>{ "generate & send proof" }</button>
            </fieldset>
            <p><b>{ "status: " }</b>{ (*status).clone() }</p>
        </main>
    }
}

/// Mount the Yew app.
pub fn run() {
    yew::Renderer::<App>::new().render();
}

#[cfg(test)]
mod tests {
    use js_sys::Array;
    use js_sys::Date;
    use js_sys::Object;
    use js_sys::Reflect;
    use rings_node::prelude::rings_core::utils::js_utils::window_sleep;
    use wasm_bindgen::JsValue;
    use wasm_bindgen_test::wasm_bindgen_test;
    use wasm_bindgen_test::wasm_bindgen_test_configure;

    use super::*;

    // Run in a real (headless) browser: `build_node` needs IndexedDB and the browser
    // WebRTC stack. Use `wasm-pack test --headless --chrome`.
    wasm_bindgen_test_configure!(run_in_browser);

    // Builds the full node in-browser — IndexedDB storage, the extension backend, and the
    // SNARK protocol registered — exercising the wasm wiring end to end (short of an
    // actual peer to prove against).
    #[wasm_bindgen_test]
    async fn builds_a_node_with_a_did() {
        let node = build_node().await.expect("build node");
        let did = node.provider.address();
        assert!(did.starts_with("0x"), "expected a DID, got {did:?}");
        node.stop();
    }

    fn obj(pairs: &[(&str, &str)]) -> JsValue {
        let object = Object::new();
        for (key, value) in pairs {
            Reflect::set(&object, &JsValue::from_str(key), &JsValue::from_str(value))
                .expect("object field set");
        }
        object.into()
    }

    fn get_str(value: &JsValue, key: &str) -> String {
        Reflect::get(value, &JsValue::from_str(key))
            .ok()
            .and_then(|field| field.as_string())
            .unwrap_or_else(|| panic!("missing string field {key:?}"))
    }

    async fn rpc(provider: &Arc<Provider>, method: &str, params: JsValue) -> JsValue {
        JsFuture::from(provider.request(method.to_string(), params))
            .await
            .unwrap_or_else(|e| panic!("rpc {method} failed: {e:?}"))
    }

    async fn connect(a: &Arc<Provider>, b: &Arc<Provider>) {
        let offer = get_str(
            &rpc(a, "createOffer", obj(&[("did", &b.address())])).await,
            "offer",
        );
        let answer = get_str(
            &rpc(b, "answerOffer", obj(&[("offer", &offer)])).await,
            "answer",
        );
        let _accepted = rpc(a, "acceptAnswer", obj(&[("answer", &answer)])).await;
    }

    async fn list_peers(provider: &Arc<Provider>) -> JsValue {
        rpc(provider, "listPeers", obj(&[])).await
    }

    fn has_connected_peer(peers_response: &JsValue, did: &str) -> bool {
        let Ok(peers) = Reflect::get(peers_response, &JsValue::from_str("peers")) else {
            return false;
        };
        let peers = Array::from(&peers);
        (0..peers.length()).any(|index| {
            let peer = peers.get(index);
            let peer_did = Reflect::get(&peer, &JsValue::from_str("did"))
                .ok()
                .and_then(|field| field.as_string());
            let state = Reflect::get(&peer, &JsValue::from_str("state"))
                .ok()
                .and_then(|field| field.as_string());
            peer_did.as_deref() == Some(did) && state.as_deref() == Some("Connected")
        })
    }

    async fn wait_connected_peer(provider: &Arc<Provider>, did: &str) {
        for _ in 0..60 {
            let peers = list_peers(provider).await;
            if has_connected_peer(&peers, did) {
                return;
            }
            window_sleep(250).await.expect("sleep");
        }
        let peers = list_peers(provider).await;
        assert!(
            has_connected_peer(&peers, did),
            "peer {did} did not reach Connected"
        );
    }

    #[wasm_bindgen_test]
    async fn two_snark_registered_nodes_connect_over_offer_answer() {
        let suffix = Date::now().to_string();
        let a = build_node_with_storage(&format!("rings-proof-demo-test-a-{suffix}"))
            .await
            .expect("build node a");
        let b = build_node_with_storage(&format!("rings-proof-demo-test-b-{suffix}"))
            .await
            .expect("build node b");

        connect(&a.provider, &b.provider).await;
        wait_connected_peer(&a.provider, &b.provider.address()).await;
        wait_connected_peer(&b.provider, &a.provider.address()).await;

        let missing_task = a
            .snark
            .get_task_result("00000000-0000-0000-0000-000000000000".to_string())
            .expect("read empty task store");
        assert_eq!(missing_task, ProofResult::Pending);
        a.stop();
        b.stop();
    }

    #[wasm_bindgen_test]
    fn sample_input_is_a_well_formed_vesta_input() {
        let input = sample_input();
        // `step_in` with two field elements, and it round-trips through JSON.
        assert_eq!(input.len(), 1);
        let (name, fields) = &input[0];
        assert_eq!(name, "step_in");
        assert_eq!(fields.len(), 2);

        let json = input.to_json().expect("to_json");
        let back = Input::from_json(json).expect("from_json");
        assert_eq!(back.len(), 1);
    }
}
