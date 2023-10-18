#![warn(missing_docs)]
//! General Client, this module provide Client implementation for FFI and WASM

use std::sync::Arc;

use rings_core::session::SessionSkBuilder;
use rings_core::storage::PersistenceStorage;

use crate::error::Error;
use crate::error::Result;
use crate::jsonrpc::handler::MethodHandler;
use crate::jsonrpc::HandlerType;
use crate::measure::PeriodicMeasure;
use crate::prelude::jsonrpc_core::types::id::Id;
use crate::prelude::jsonrpc_core::MethodCall;
use crate::prelude::wasm_export;
use crate::prelude::CallbackFn;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

#[cfg(feature = "browser")]
pub mod browser;
#[cfg(feature = "ffi")]
pub mod ffi;

/// General Client, which holding reference of Processor and MessageCallback Handler
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

#[allow(dead_code)]
impl Client {
    /// Create a client instance with storage name
    pub(crate) async fn new_client_with_storage_internal(
        config: ProcessorConfig,
        cb: Option<CallbackFn>,
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

        let mut processor_builder = ProcessorBuilder::from_config(&config)?
            .storage(storage)
            .measure(measure);

        if let Some(cb) = cb {
            processor_builder = processor_builder.message_callback(cb);
        }

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
        callback: Option<CallbackFn>,
        storage_name: String,
    ) -> Result<Client> {
        let config: ProcessorConfig = serde_yaml::from_str(&config)?;
        Self::new_client_with_storage_internal(config, callback, storage_name).await
    }

    /// Create a new client instanice with everything in detail
    /// Ice_servers should obey forrmat: "[turn|strun]://<Address>:<Port>;..."
    /// Account is hex string
    /// Account should format as same as account_type declared
    /// Account_type is lowercase string, possible input are: `eip191`, `ed25519`, `bip137`, for more imformation,
    /// please check [rings_core::ecc]
    /// Signer should accept a String and returns bytes.
    /// Signer should function as same as account_type declared, Eg: eip191 or secp256k1 or ed25519.
    /// callback should be an instance of [CallbackFn]
    pub(crate) async fn new_client_internal(
        ice_servers: String,
        stabilize_timeout: usize,
        account: String,
        account_type: String,
        signer: Box<dyn Fn(String) -> Vec<u8>>,
        callback: Option<CallbackFn>,
    ) -> Result<Client> {
        let mut sk_builder = SessionSkBuilder::new(account, account_type);
        let proof = sk_builder.unsigned_proof();
        let sig = signer(proof);
        sk_builder = sk_builder.set_session_sig(sig.to_vec());
        let session_sk = sk_builder.build().map_err(Error::InternalError)?;
        let config = ProcessorConfig::new(ice_servers, session_sk, stabilize_timeout);
        Self::new_client_with_storage_internal(config, callback, "rings-node".to_string()).await
    }

    /// Request local rpc interface
    /// the internal rpc interface is provide by rings_rpc
    pub(crate) async fn request_internal(
        &self,
        method: String,
        params: String,
        opt_id: Option<String>,
    ) -> Result<String> {
        let handler = self.handler.clone();
        let params = serde_json::from_str(&params)?;
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
        serde_json::to_string(
            &handler
                .handle_request(req)
                .await
                .map_err(Error::InternalRpcError)?,
        )
        .map_err(Error::SerdeJsonError)
    }
}
