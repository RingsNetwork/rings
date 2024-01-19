#![warn(missing_docs)]
//! General Provider, this module provide Provider implementation for FFI and WASM

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rings_core::dht::VNodeStorage;
use rings_core::session::SessionSkBuilder;
use rings_core::storage::MemStorage;
use rings_core::swarm::callback::SharedSwarmCallback;
use rings_rpc::protos::rings_node_handler::InternalRpcHandler;

use crate::backend::types::BackendMessage;
use crate::backend::types::MessageHandler;
use crate::backend::Backend;
use crate::error::Error;
use crate::error::Result;
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
        Self {
            processor,
            handler: InternalRpcHandler,
        }
    }
    /// Create a provider instance with storage name
    pub(crate) async fn new_provider_with_storage_internal(
        config: ProcessorConfig,
        vnode_storage: Option<VNodeStorage>,
        measure_storage: Option<MeasureStorage>,
    ) -> Result<Provider> {
        let vnode_storage = vnode_storage.unwrap_or_else(|| Box::new(MemStorage::new()));
        let measure_storage = measure_storage.unwrap_or_else(|| Box::new(MemStorage::new()));

        let measure = PeriodicMeasure::new(measure_storage);

        let processor_builder = ProcessorBuilder::from_config(&config)?
            .storage(vnode_storage)
            .measure(measure);

        let processor = Arc::new(processor_builder.build()?);

        Ok(Provider {
            processor,
            handler: InternalRpcHandler,
        })
    }

    /// Create a new provider instanice with everything in detail
    /// Ice_servers should obey forrmat: "[turn|strun]://<Address>:<Port>;..."
    /// Account is hex string
    /// Account should format as same as account_type declared
    /// Account_type is lowercase string, possible input are: `eip191`, `ed25519`, `bip137`, for more imformation,
    /// please check [rings_core::ecc]
    /// Signer should accept a String and returns bytes.
    /// Signer should function as same as account_type declared, Eg: eip191 or secp256k1 or ed25519.
    pub(crate) async fn new_provider_internal(
        ice_servers: String,
        stabilize_timeout: u64,
        account: String,
        account_type: String,
        signer: Signer,
        vnode_storage: Option<VNodeStorage>,
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
        let config = ProcessorConfig::new(ice_servers, session_sk, stabilize_timeout);
        Self::new_provider_with_storage_internal(config, vnode_storage, measure_storage).await
    }

    /// Set callback for swarm, it can be T, or (T0, T1, T2)
    /// Where T is MessageHandler<BackendMessage> + Send + Sync + Sized + 'static,
    pub fn set_backend_callback<T>(&self, callback: T) -> Result<()>
    where T: MessageHandler<BackendMessage> + Send + Sync + Sized + 'static {
        let backend = Backend::new(Arc::new(self.clone()), Box::new(callback));
        self.processor
            .swarm
            .set_callback(Arc::new(backend))
            .map_err(Error::InternalError)
    }

    /// Set callback for swarm.
    pub(crate) fn set_swarm_callback(&self, callback: SharedSwarmCallback) -> Result<()> {
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
        tracing::debug!("request {} params: {:?}", method, params);
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
