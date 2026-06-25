#![warn(missing_docs)]
//! General Provider, this module provide Provider implementation for FFI and WASM

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rings_core::dht::EntryStorage;
use rings_core::session::SessionSkBuilder;
use rings_core::storage::MemStorage;
use rings_core::swarm::callback::SharedSwarmCallback;
use rings_rpc::protos::rings_node_handler::InternalRpcHandler;

use crate::error::Error;
use crate::error::Result;
use crate::extension::Backend;
use crate::measure::MeasureStorage;
use crate::measure::PeriodicMeasure;
use crate::prelude::wasm_export;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

#[cfg(feature = "browser")]
pub mod browser;
#[cfg(feature = "ffi")]
pub mod ffi;

/// General Provider, which holding reference of Processor
/// Provider should be obey memory layout of CLang
/// Provider should be export for wasm-bindgen
#[derive(Clone)]
#[allow(dead_code)]
#[repr(C)]
#[wasm_export]
pub struct Provider {
    processor: Arc<Processor>,
    handler: InternalRpcHandler,
    extensions: crate::extension::ext::Extensions,
}

/// Async signer, without Send required
#[cfg(feature = "browser")]
pub type AsyncSigner = Box<dyn Fn(String) -> Pin<Box<dyn Future<Output = Vec<u8>>>>>;

/// Async signer, use for non-wasm envirement, Send is necessary
#[cfg(not(feature = "browser"))]
pub type AsyncSigner = Box<dyn Fn(String) -> Pin<Box<dyn Future<Output = Vec<u8>> + Send>>>;

/// Signer can be async and sync
#[allow(clippy::type_complexity)]
pub enum Signer {
    /// Sync signer
    Sync(Box<dyn Fn(String) -> Vec<u8>>),
    /// Async signer
    Async(AsyncSigner),
}

#[allow(dead_code)]
impl Provider {
    /// Create provider from processor directly
    pub fn from_processor(processor: Arc<Processor>) -> Self {
        let extensions = crate::extension::ext::Extensions::new(processor.clone());
        Self {
            processor,
            handler: InternalRpcHandler,
            extensions,
        }
    }

    /// The shared protocol registry. The inbound callback clones this so
    /// registration (via the provider) and dispatch see the same table.
    pub fn extensions(&self) -> crate::extension::ext::Extensions {
        self.extensions.clone()
    }

    /// The capability handle — overlay `send` / `did` / self-addressed `inject`. (Authenticated
    /// `dispatch` is router-only; `pub(crate)` so it never reaches public callers.)
    pub(crate) fn core(&self) -> crate::extension::ext::Core {
        self.extensions.core()
    }

    /// Register a pure [`Protocol`](crate::extension::ext::Protocol) together with its
    /// [`Interpret`](crate::extension::ext::Interpret) shell under the protocol's namespace.
    /// Errors if the namespace is already taken.
    pub fn register_protocol<P, I>(&self, protocol: P, interpret: I) -> Result<()>
    where
        P: crate::extension::ext::Protocol + crate::extension::ext::MaybeSend + 'static,
        P::State: crate::extension::ext::MaybeSend + 'static,
        P::Effect: crate::extension::ext::MaybeSend,
        I: crate::extension::ext::Interpret<Effect = P::Effect>
            + crate::extension::ext::MaybeSend
            + 'static,
    {
        self.extensions.register(protocol, interpret)
    }

    /// Send a namespaced payload to a peer. This is the uniform upper-layer send — a core
    /// capability, identical on native and browser.
    pub async fn send(
        &self,
        to: rings_core::dht::Did,
        namespace: &str,
        payload: bytes::Bytes,
    ) -> Result<()> {
        self.core().send(to, namespace, payload).await
    }
    /// Create a provider instance with storage name
    pub(crate) async fn new_provider_with_storage_internal(
        config: ProcessorConfig,
        entry_storage: Option<EntryStorage>,
        measure_storage: Option<MeasureStorage>,
    ) -> Result<Provider> {
        let entry_storage = entry_storage.unwrap_or_else(|| Box::new(MemStorage::new()));
        let measure_storage = measure_storage.unwrap_or_else(|| Box::new(MemStorage::new()));

        let measure = PeriodicMeasure::new(measure_storage);

        let processor_builder = ProcessorBuilder::from_config(&config)?
            .storage(entry_storage)
            .measure(measure);

        let processor = Arc::new(processor_builder.build()?);

        let extensions = crate::extension::ext::Extensions::new(processor.clone());

        Ok(Provider {
            processor,
            handler: InternalRpcHandler,
            extensions,
        })
    }

    /// Create a new provider instanice with everything in detail
    /// Ice_servers should obey forrmat: `"[turn|strun]://<Address>:<Port>;..."`
    /// Account is hex string
    /// Account should format as same as account_type declared
    /// Account_type is lowercase string, possible input are: `eip191`, `ed25519`, `bip137`, for more information,
    /// please check [rings_core::ecc]
    /// Signer should accept a String and returns bytes.
    /// Signer should function as same as account_type declared, Eg: eip191 or secp256k1 or ed25519.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new_provider_internal(
        network_id: u32,
        ice_servers: String,
        stabilize_interval: u64,
        account: String,
        account_type: String,
        signer: Signer,
        entry_storage: Option<EntryStorage>,
        measure_storage: Option<MeasureStorage>,
    ) -> Result<Provider> {
        let mut sk_builder = SessionSkBuilder::new(account, account_type);
        let proof = sk_builder.unsigned_proof();
        let sig = match signer {
            Signer::Sync(s) => s(proof),
            Signer::Async(s) => s(proof).await,
        };
        sk_builder = sk_builder.set_session_sig(sig.to_vec());
        let session_sk = sk_builder.build().map_err(Error::InternalError)?;
        let config = ProcessorConfig::new(network_id, ice_servers, session_sk, stabilize_interval);
        Self::new_provider_with_storage_internal(config, entry_storage, measure_storage).await
    }

    /// Install the extension [`Backend`] as the swarm's inbound callback, so inbound
    /// custom messages are decoded as [`Envelope`](crate::extension::ext::Envelope)s and
    /// routed to their namespace's protocol. Call once after registering protocols.
    pub fn set_backend(&self) -> Result<()> {
        let backend = Backend::new(Arc::new(self.clone()));
        self.processor
            .swarm
            .set_callback(Arc::new(backend))
            .map_err(Error::InternalError)
    }

    /// Set callback for swarm.
    #[deprecated(
        note = "set_swarm_callback will be removed in next version, plz use set_backend instead"
    )]
    pub fn set_swarm_callback(&self, callback: SharedSwarmCallback) -> Result<()> {
        self.processor
            .swarm
            .set_callback(callback)
            .map_err(Error::InternalError)
    }

    pub(crate) fn set_swarm_callback_internal(&self, callback: SharedSwarmCallback) -> Result<()> {
        self.processor
            .swarm
            .set_callback(callback)
            .map_err(Error::InternalError)
    }

    /// Request local rpc interface
    /// the internal rpc interface is provide by rings_rpc
    pub async fn request_internal(
        &self,
        method: String,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        tracing::debug!("request {}", method);
        self.handler
            .handle_request(self.processor.clone(), method, params)
            .await
            .map_err(Error::InternalRpcError)
    }
}

#[cfg(feature = "node")]
impl Provider {
    /// A request function implementation for native provider
    pub async fn request<T>(
        &self,
        method: rings_rpc::method::Method,
        params: T,
    ) -> Result<serde_json::Value>
    where
        T: serde::Serialize,
    {
        let params = serde_json::to_value(params)?;
        self.request_internal(method.to_string(), params).await
    }

    /// Listen messages
    pub async fn listen(&self) {
        self.processor.listen().await;
    }
}
