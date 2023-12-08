#![warn(missing_docs)]
//! General Client, this module provide Client implementation for FFI and WASM

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rings_core::session::SessionSkBuilder;
use rings_core::storage::PersistenceStorage;
use rings_core::swarm::callback::SharedSwarmCallback;

use crate::error::Error;
use crate::error::Result;
use crate::jsonrpc::handler::MethodHandler;
use crate::jsonrpc::HandlerType;
use crate::measure::PeriodicMeasure;
use crate::prelude::jsonrpc_core::types::id::Id;
use crate::prelude::jsonrpc_core::MethodCall;
use crate::prelude::jsonrpc_core::Output;
use crate::prelude::jsonrpc_core::Params;
use crate::prelude::wasm_export;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

#[cfg(feature = "browser")]
pub mod browser;
#[cfg(feature = "ffi")]
pub mod ffi;

/// General Client, which holding reference of Processor
/// Client should be obey memory layout of CLang
/// Client should be export for wasm-bindgen
#[derive(Clone)]
#[allow(dead_code)]
#[repr(C)]
#[wasm_export]
pub struct Client {
    processor: Arc<Processor>,
    handler: Arc<HandlerType>,
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
impl Client {
    /// Create client from processor directly
    pub fn from_processor(processor: Arc<Processor>) -> Self {
        let mut handler: HandlerType = processor.clone().into();
        handler.build();
        Self {
            processor,
            handler: handler.into(),
        }
    }
    /// Create a client instance with storage name
    pub(crate) async fn new_client_with_storage_internal(
        config: ProcessorConfig,
        storage_name: String,
    ) -> Result<Client> {
        let storage_path = storage_name.as_str();
        let measure_path = [storage_path, "measure"].join("/");

        let storage = PersistenceStorage::new_with_cap_and_name(50000, storage_path)
            .await
            .map_err(Error::Storage)?;

        let ms = PersistenceStorage::new_with_cap_and_path(50000, measure_path)
            .await
            .map_err(Error::Storage)?;
        let measure = PeriodicMeasure::new(ms);

        let processor_builder = ProcessorBuilder::from_config(&config)?
            .storage(storage)
            .measure(measure);

	#[allow(clippy::arc_with_non_send_sync)]
        let processor = Arc::new(processor_builder.build()?);

        let mut handler: HandlerType = processor.clone().into();
        handler.build();

        Ok(Client {
            processor,
            handler: handler.into(),
        })
    }

    /// Create a client instance with storage name and serialized config string
    /// This function is useful for creating a client with config file (yaml and json).
    pub(crate) async fn new_client_with_storage_and_serialized_config_internal(
        config: String,
        storage_name: String,
    ) -> Result<Client> {
        let config: ProcessorConfig = serde_yaml::from_str(&config)?;
        Self::new_client_with_storage_internal(config, storage_name).await
    }

    /// Create a new client instanice with everything in detail
    /// Ice_servers should obey forrmat: "[turn|strun]://<Address>:<Port>;..."
    /// Account is hex string
    /// Account should format as same as account_type declared
    /// Account_type is lowercase string, possible input are: `eip191`, `ed25519`, `bip137`, for more imformation,
    /// please check [rings_core::ecc]
    /// Signer should accept a String and returns bytes.
    /// Signer should function as same as account_type declared, Eg: eip191 or secp256k1 or ed25519.
    pub(crate) async fn new_client_internal(
        ice_servers: String,
        stabilize_timeout: usize,
        account: String,
        account_type: String,
        signer: Signer,
    ) -> Result<Client> {
        let mut sk_builder = SessionSkBuilder::new(account, account_type);
        let proof = sk_builder.unsigned_proof();
        let sig = match signer {
            Signer::Sync(s) => s(proof),
            Signer::Async(s) => s(proof).await,
        };
        sk_builder = sk_builder.set_session_sig(sig.to_vec());
        let session_sk = sk_builder.build().map_err(Error::InternalError)?;
        let config = ProcessorConfig::new(ice_servers, session_sk, stabilize_timeout);
        Self::new_client_with_storage_internal(config, "rings-node".to_string()).await
    }

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
        params: Params,
        opt_id: Option<String>,
    ) -> Result<Output> {
        let handler = self.handler.clone();
        let id = if let Some(id) = opt_id {
            Id::Str(id)
        } else {
            Id::Null
        };
        let req: MethodCall = MethodCall {
            jsonrpc: None,
            method,
            params,
            id,
        };
        handler
            .handle_request(req)
            .await
            .map_err(Error::InternalRpcError)
    }
}

#[cfg(feature = "node")]
impl Client {
    /// A request function implementation for native client
    pub async fn request(
        &self,
        method: String,
        params: Params,
        opt_id: Option<String>,
    ) -> Result<Output> {
        self.request_internal(method, params, opt_id).await
    }
}
