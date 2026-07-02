//! Browser node construction and local RPC helpers.

use std::sync::Arc;

use futures::future::AbortHandle;
use futures::future::Abortable;
use js_sys::Array;
use js_sys::Object;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_node::extension::snark::SNARKBehaviour;
use rings_node::prelude::rings_core::session::SessionSkBuilder;
use rings_node::prelude::rings_core::storage::idb::IdbStorage;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_futures::JsFuture;

use crate::wallet::WalletAccount;

/// A browser Rings node with all demo protocols installed.
#[derive(Clone)]
pub struct DemoNode {
    /// Local provider handle.
    pub provider: Arc<Provider>,
    /// SNARK behaviour and task store.
    pub snark: SNARKBehaviour,
    listen_abort: AbortHandle,
}

impl DemoNode {
    /// Stop the background listen/stabilize loop started for this demo node.
    pub fn stop(&self) {
        self.listen_abort.abort();
    }
}

/// Peer entry rendered in the topology panel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerView {
    /// Peer DID.
    pub did: String,
    /// Transport state reported by `listPeers`.
    pub state: String,
}

/// User-controlled node startup settings.
pub struct NodeSettings {
    /// Rings network id.
    pub network_id: u32,
    /// ICE server list.
    pub ice_servers: String,
    /// Stabilization interval in seconds.
    pub stabilize_interval: u64,
    /// IndexedDB storage namespace.
    pub storage_name: String,
}

/// Build a browser provider from a wallet-authorized session key.
#[allow(clippy::arc_with_non_send_sync)]
pub async fn build_node(
    wallet: &WalletAccount,
    settings: NodeSettings,
) -> Result<DemoNode, String> {
    let mut builder = SessionSkBuilder::new(wallet.account.clone(), wallet.account_type.clone());
    let proof = builder.unsigned_proof();
    let signature = wallet.sign_session_proof(&proof).await?;
    builder = builder.set_session_sig(signature);
    let session_sk = builder
        .build()
        .map_err(|error| format!("session key rejected: {error}"))?;
    let config = ProcessorConfig::new(
        settings.network_id,
        settings.ice_servers,
        session_sk,
        settings.stabilize_interval,
    );
    let storage = Box::new(
        IdbStorage::new_with_cap_and_name(50_000, &settings.storage_name)
            .await
            .map_err(|error| format!("idb storage: {error}"))?,
    );
    let processor = Arc::new(
        ProcessorBuilder::from_config(&config)
            .map_err(|error| format!("processor builder: {error}"))?
            .storage(storage)
            .build()
            .map_err(|error| format!("build processor: {error}"))?,
    );
    let provider = Arc::new(Provider::from_processor(processor));
    provider
        .set_backend()
        .map_err(|error| format!("install backend: {error}"))?;

    let snark = SNARKBehaviour::default();
    snark
        .register(&provider)
        .map_err(|error| format!("register snark protocol: {error}"))?;

    let listening = provider.clone();
    let (listen_abort, listen_registration) = AbortHandle::new_pair();
    spawn_local(async move {
        let listen = async move {
            let _result = JsFuture::from(listening.listen()).await;
        };
        let _result = Abortable::new(listen, listen_registration).await;
    });

    Ok(DemoNode {
        provider,
        snark,
        listen_abort,
    })
}

/// Connect to a seed node through its HTTP JSON-RPC endpoint.
pub async fn connect_http(provider: &Arc<Provider>, endpoint: String) -> Result<String, String> {
    let response = request(
        provider,
        "connectPeerViaHttp",
        obj(&[("url", endpoint.as_str())]),
    )
    .await?;
    get_string(&response, "did")
}

/// Create an SDP offer for a remote DID.
pub async fn create_offer(provider: &Arc<Provider>, did: String) -> Result<String, String> {
    let response = request(provider, "createOffer", obj(&[("did", did.as_str())])).await?;
    get_string(&response, "offer")
}

/// Answer an SDP offer and return the answer payload.
pub async fn answer_offer(provider: &Arc<Provider>, offer: String) -> Result<String, String> {
    let response = request(provider, "answerOffer", obj(&[("offer", offer.as_str())])).await?;
    get_string(&response, "answer")
}

/// Accept a remote SDP answer.
pub async fn accept_answer(provider: &Arc<Provider>, answer: String) -> Result<(), String> {
    request(
        provider,
        "acceptAnswer",
        obj(&[("answer", answer.as_str())]),
    )
    .await
    .map(|_| ())
}

/// Disconnect all currently known peers from a local provider.
pub async fn disconnect_all(provider: &Arc<Provider>) -> Result<usize, String> {
    let peers = list_peers(provider).await?;
    let mut closed = 0;
    let mut attempted = 0;
    for peer in peers {
        if peer.did == "unknown" {
            continue;
        }
        attempted += 1;
        if request(provider, "disconnect", obj(&[("did", peer.did.as_str())]))
            .await
            .is_ok()
        {
            closed += 1;
        }
    }
    if attempted == closed {
        Ok(closed)
    } else {
        Err(format!("closed {closed}/{attempted} peer links"))
    }
}

/// Send a namespace-scoped payload to a remote DID.
pub async fn send_message(
    provider: Arc<Provider>,
    did: String,
    namespace: String,
    payload: Vec<u8>,
) -> Result<(), String> {
    JsFuture::from(provider.send_message(did, namespace, Uint8Array::from(payload.as_slice())))
        .await
        .map(|_| ())
        .map_err(|error| format!("send message failed: {error:?}"))
}

/// Refresh connected peers.
pub async fn list_peers(provider: &Arc<Provider>) -> Result<Vec<PeerView>, String> {
    let response = request(provider, "listPeers", Object::new().into()).await?;
    let peers = Reflect::get(&response, &JsValue::from_str("peers"))
        .map_err(|error| format!("read peers failed: {error:?}"))?;
    let peers = Array::from(&peers);
    let mut out = Vec::new();
    for index in 0..peers.length() {
        let peer = peers.get(index);
        let did = get_string(&peer, "did").unwrap_or_else(|_| "unknown".to_string());
        let state = get_string(&peer, "state").unwrap_or_else(|_| "Unknown".to_string());
        out.push(PeerView { did, state });
    }
    Ok(out)
}

async fn request(
    provider: &Arc<Provider>,
    method: &str,
    params: JsValue,
) -> Result<JsValue, String> {
    JsFuture::from(provider.request(method.to_string(), params))
        .await
        .map_err(|error| format!("rpc {method} failed: {error:?}"))
}

fn obj(fields: &[(&str, &str)]) -> JsValue {
    let object = Object::new();
    for (key, value) in fields {
        let _set = Reflect::set(&object, &JsValue::from_str(key), &JsValue::from_str(value));
    }
    object.into()
}

fn get_string(value: &JsValue, field: &str) -> Result<String, String> {
    Reflect::get(value, &JsValue::from_str(field))
        .map_err(|error| format!("read {field} failed: {error:?}"))?
        .as_string()
        .ok_or_else(|| format!("missing string field {field}"))
}
