//! Browser node construction and local RPC helpers.

use std::sync::Arc;

use js_sys::Array;
use js_sys::Object;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_node::extension::snark::SNARKBehaviour;
use rings_node::prelude::rings_core::session::SessionSkBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::browser::ProviderListener;
use rings_node::provider::Provider;
use rings_webview::Result as WebviewResult;
use rings_webview::WebviewError;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use crate::wallet::WalletAccount;
use crate::webview::WebviewNode;
use crate::webview::WebviewOnionSettings;

/// A browser Rings node with all demo protocols installed.
#[derive(Clone)]
pub struct DemoNode {
    /// Local provider handle.
    pub provider: Arc<Provider>,
    /// SNARK behaviour and task store.
    pub snark: SNARKBehaviour,
    /// Controlled webview gateway attached to this browser node when its host origin is HTTP(S).
    pub webview: Option<WebviewNode>,
    listener: ProviderListener,
}

impl DemoNode {
    /// Return true when both handles refer to the same browser provider instance.
    pub(crate) fn same_provider_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.provider, &other.provider)
    }

    /// Stop the background listen/stabilize loop started for this demo node.
    pub fn stop(&self) {
        self.listener.stop();
    }

    /// Return the controlled webview gateway attached to this browser node.
    pub fn webview(&self) -> WebviewResult<WebviewNode> {
        self.webview.clone().ok_or_else(|| {
            WebviewError::Browser(
                "webview requires an HTTP(S) frontend origin; it is unavailable in this host"
                    .to_string(),
            )
        })
    }
}

/// Peer entry rendered in the topology panel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerView {
    /// Peer DID.
    did: String,
    /// Transport state reported by `listPeers`.
    state: String,
}

impl PeerView {
    /// Build a peer row only when the provider returned an addressable DID.
    pub fn from_fields(did: String, state: String) -> Option<Self> {
        if did.trim().is_empty() {
            return None;
        }
        Some(Self { did, state })
    }

    /// Build a connected peer row from an RPC-returned DID.
    pub fn connected(did: String) -> Option<Self> {
        Self::from_fields(did, "Connected".to_string())
    }

    /// Peer DID.
    pub fn did(&self) -> &str {
        &self.did
    }

    /// Transport state reported by `listPeers`.
    pub fn state(&self) -> &str {
        &self.state
    }

    /// True when this row can be used as a peer operation target.
    pub fn is_addressable(&self) -> bool {
        !self.did.trim().is_empty()
    }
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
    /// Runtime WebView onion routing settings.
    pub webview_onion_settings: WebviewOnionSettings,
}

/// Build a browser provider from a wallet-authorized session key.
///
/// The browser provider is used only on the single-threaded wasm event loop, but
/// the upstream `Provider` handle is exposed behind `Arc`; keep that shape at
/// this adapter boundary instead of introducing a parallel wasm-only provider.
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
    let provider = Arc::new(
        Provider::new_browser_provider_with_storage(config, settings.storage_name)
            .await
            .map_err(|error| format!("build provider: {error}"))?,
    );

    let snark = SNARKBehaviour::default();
    snark
        .register(&provider)
        .map_err(|error| format!("register snark protocol: {error}"))?;
    let webview =
        WebviewNode::for_current_window(provider.clone(), settings.webview_onion_settings)
            .map_err(|error| format!("initialize webview: {error}"))?;
    let listener = provider.listen();

    Ok(DemoNode {
        provider,
        snark,
        webview,
        listener,
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
        attempted += 1;
        if request(provider, "disconnect", obj(&[("did", peer.did())]))
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
        let did = get_string(&peer, "did").unwrap_or_default();
        let state = get_string(&peer, "state").unwrap_or_else(|_| "Unknown".to_string());
        if let Some(peer) = PeerView::from_fields(did, state) {
            out.push(peer);
        }
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::FutureExt;
    use gloo_timers::future::sleep;
    use rings_node::prelude::rings_core::ecc::SecretKey;
    use rings_node::prelude::rings_core::prelude::uuid;
    use rings_node::prelude::rings_core::session::SessionSk;
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::*;

    const TEST_NETWORK_ID: u32 = 665;
    const TEST_ICE_SERVERS: &str = "stun://stun.l.google.com:19302";
    const LISTENER_START_TIMEOUT_MS: u64 = 2_000;
    const LISTENER_SETTLE_TIMEOUT_MS: u64 = 2_000;

    #[wasm_bindgen_test(async)]
    async fn demo_node_stop_settles_provider_listener_task() {
        let result = run_demo_node_stop_settles_provider_listener_task().await;
        assert!(
            result.is_ok(),
            "DemoNode::stop did not settle ProviderListener task: {result:?}"
        );
    }

    // Mirrors the browser-only `DemoNode` ownership boundary from `build_node`.
    #[allow(clippy::arc_with_non_send_sync)]
    async fn run_demo_node_stop_settles_provider_listener_task() -> Result<(), String> {
        let key = SecretKey::random();
        let node = build_test_demo_node(&key).await?;
        let started = node.listener.started();
        let task = node.listener.task();

        assert!(!node.listener.is_stopped());
        await_promise_with_timeout(
            started,
            LISTENER_START_TIMEOUT_MS,
            "ProviderListener did not start",
        )
        .await?;
        sleep(Duration::from_millis(20)).await;
        assert!(!node.listener.is_stopped());
        node.stop();
        assert!(node.listener.is_stopped());
        await_promise_with_timeout(
            task,
            LISTENER_SETTLE_TIMEOUT_MS,
            "ProviderListener task did not settle",
        )
        .await
    }

    #[wasm_bindgen_test(async)]
    async fn demo_node_identity_distinguishes_same_wallet_restarts() {
        let result = run_demo_node_identity_distinguishes_same_wallet_restarts().await;
        assert!(
            result.is_ok(),
            "DemoNode identity should not collapse same-wallet restarts: {result:?}"
        );
    }

    #[allow(clippy::arc_with_non_send_sync)]
    async fn run_demo_node_identity_distinguishes_same_wallet_restarts() -> Result<(), String> {
        let key = SecretKey::random();
        let first = build_test_demo_node(&key).await?;
        let second = build_test_demo_node(&key).await?;

        assert_eq!(first.provider.address(), second.provider.address());
        assert!(first.same_provider_instance(&first.clone()));
        assert!(!first.same_provider_instance(&second));

        first.stop();
        second.stop();
        await_promise_with_timeout(
            first.listener.task(),
            LISTENER_SETTLE_TIMEOUT_MS,
            "first ProviderListener task did not settle",
        )
        .await?;
        await_promise_with_timeout(
            second.listener.task(),
            LISTENER_SETTLE_TIMEOUT_MS,
            "second ProviderListener task did not settle",
        )
        .await
    }

    // Mirrors the browser-only `DemoNode` ownership boundary from `build_node`.
    #[allow(clippy::arc_with_non_send_sync)]
    async fn build_test_demo_node(key: &SecretKey) -> Result<DemoNode, String> {
        let session_sk = SessionSk::new_with_seckey(key)
            .map_err(|error| format!("session key rejected: {error}"))?;
        let config =
            ProcessorConfig::new(TEST_NETWORK_ID, TEST_ICE_SERVERS.to_string(), session_sk, 0);
        let storage_name = format!(
            "rings-frontend-listener-{}",
            uuid::Uuid::new_v4().to_simple()
        );
        let provider = Arc::new(
            Provider::new_browser_provider_with_storage(config, storage_name)
                .await
                .map_err(|error| format!("build provider: {error}"))?,
        );
        let listener = provider.listen();
        let node = DemoNode {
            provider,
            snark: SNARKBehaviour::default(),
            webview: None,
            listener,
        };
        Ok(node)
    }

    async fn await_promise_with_timeout(
        promise: js_sys::Promise,
        timeout_ms: u64,
        timeout_message: &str,
    ) -> Result<(), String> {
        let promise = JsFuture::from(promise)
            .map(|result| {
                result
                    .map(|_| ())
                    .map_err(crate::browser_api::js_error_label)
            })
            .fuse();
        let timeout = sleep(Duration::from_millis(timeout_ms))
            .map(|_| Err(format!("{timeout_message} within {timeout_ms}ms")))
            .fuse();
        futures::pin_mut!(promise, timeout);
        futures::select! {
            result = promise => result,
            result = timeout => result,
        }
    }
}
