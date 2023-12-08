#![warn(missing_docs)]
//! JSON-RPC handler for both feature=browser and feature=node.
//! We support running the JSON-RPC server in either native or browser environment.
//! For the native environment, we use jsonrpc_core to handle requests.
//! For the browser environment, we utilize a Simple MessageHandler to process the requests.
use core::future::Future;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;

#[cfg(feature = "node")]
pub use self::default::build_handler;
use super::server;
use super::server::RpcMeta;
use crate::prelude::jsonrpc_core::types::error::Error;
use crate::prelude::jsonrpc_core::types::error::ErrorCode;
use crate::prelude::jsonrpc_core::types::request::MethodCall;
use crate::prelude::jsonrpc_core::types::response::Output;
use crate::prelude::jsonrpc_core::Params;
use crate::prelude::jsonrpc_core::Result;
use crate::prelude::jsonrpc_core::Value;
use crate::prelude::rings_rpc::method::Method;
use crate::processor::Processor;

/// Type of handler function
#[cfg(feature = "node")]
pub type MethodFnBox = Box<
    dyn Fn(Params, RpcMeta) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> + Send + Sync,
>;

/// Type of handler function, note that, in browser environment there is no Send and Sync
#[cfg(feature = "browser")]
pub type MethodFnBox = Box<dyn Fn(Params, RpcMeta) -> Pin<Box<dyn Future<Output = Result<Value>>>>>;

/// This macro is used to transform an asynchronous function item into
/// a boxed function reference that returns a pinned future.
macro_rules! pin {
    ($fn:path) => {
        Box::new(|params, meta| Box::pin($fn(params, meta)))
    };
}

/// The InternalRPCHandler is
/// consistent with the signature type MetaIoHandler in jsonrpc_core,
/// but it is more simpler and does not have Send and Sync bounds.
/// Here, the type T is likely to be RpcMeta indefinitely.
#[derive(Clone)]
pub struct InternalRPCHandler<T: Clone> {
    /// MetaData for jsonrpc
    meta: T,
    /// Registered Methods
    methods: HashMap<String, Arc<MethodFnBox>>,
}

/// This trait defines the register function for method registration.
pub trait MethodRegister {
    /// Registers a method with the given name and function.
    fn register(&mut self, name: &str, func: MethodFnBox);
}

/// A trait that defines the method handler for handling method requests.
#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
pub trait MethodHandler<T> {
    /// Handles the incoming method request and returns the result.
    /// Please note that the function type here is MethodCall instead of
    /// Request in jsonrpc_core, which means that batch requests are not supported here.
    async fn handle_request(&self, request: MethodCall) -> Result<Output>;
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl MethodHandler<server::RpcMeta> for InternalRPCHandler<server::RpcMeta> {
    async fn handle_request(&self, request: MethodCall) -> Result<Output> {
        let output: Result<Value> = if let Some(handler) = self.methods.get(&request.method) {
            let ret = handler(request.params, self.meta.clone()).await?;
            Ok(ret)
        } else {
            Err(Error {
                code: ErrorCode::MethodNotFound,
                message: format!("method {} is not found", &request.method),
                data: None,
            })
        };
        Ok(Output::from(output, request.id, None))
    }
}

impl<T: Clone> MethodRegister for InternalRPCHandler<T> {
    fn register(&mut self, name: &str, func: MethodFnBox) {
	#[allow(clippy::arc_with_non_send_sync)]
        self.methods.insert(name.to_string(), Arc::new(func));
    }
}

impl InternalRPCHandler<server::RpcMeta> {
    /// Create a new instance of message handler
    pub fn new(meta: server::RpcMeta) -> Self {
        Self {
            meta,
            methods: HashMap::new(),
        }
    }

    /// Register all methods
    pub fn build(&mut self) {
        for m in methods() {
            self.register(m.0.as_str(), m.1);
        }
    }
}

impl From<Arc<Processor>> for HandlerType {
    fn from(p: Arc<Processor>) -> Self {
        let meta: server::RpcMeta = p.into();
        HandlerType::new(meta)
    }
}

/// Type alice for InternalRpcHandler
pub type HandlerType = InternalRPCHandler<server::RpcMeta>;

/// This function will return a list of public functions for all interfaces.
/// If you need to define interfaces separately for the browser or native,
/// you should use cfg to control the conditions.
pub fn methods() -> Vec<(Method, MethodFnBox)> {
    vec![
        (
            Method::ConnectPeerViaHttp,
            pin!(server::connect_peer_via_http),
        ),
        (
            Method::ConnectPeerViaHttp,
            pin!(server::connect_peer_via_http),
        ),
        (Method::ConnectWithSeed, pin!(server::connect_with_seed)),
        (Method::AnswerOffer, pin!(server::answer_offer)),
        (Method::ConnectWithDid, pin!(server::connect_with_did)),
        (Method::CreateOffer, pin!(server::create_offer)),
        (Method::AcceptAnswer, pin!(server::accept_answer)),
        (Method::ListPeers, pin!(server::list_peers)),
        (Method::Disconnect, pin!(server::close_connection)),
        (Method::SendTo, pin!(server::send_raw_message)),
        (Method::SendCustomMessage, pin!(server::send_custom_message)),
        (
            Method::SendBackendMessage,
            pin!(server::send_backend_message),
        ),
        (
            Method::PublishMessageToTopic,
            pin!(server::publish_message_to_topic),
        ),
        (
            Method::FetchMessagesOfTopic,
            pin!(server::fetch_messages_of_topic),
        ),
        (Method::RegisterService, pin!(server::register_service)),
        (Method::LookupService, pin!(server::lookup_service)),
        (Method::NodeInfo, pin!(server::node_info)),
        (Method::NodeDid, pin!(server::node_did)),
    ]
}

/// Implementation for native node
#[cfg(feature = "node")]
pub mod default {
    use super::*;
    use crate::prelude::jsonrpc_core::MetaIoHandler;

    /// Build handler add method with metadata.
    pub async fn build_handler(handler: &mut MetaIoHandler<server::RpcMeta>) {
        for m in methods() {
            handler.add_method_with_meta(m.0.as_str(), m.1);
        }
    }
}
