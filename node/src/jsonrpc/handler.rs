#![warn(missing_docs)]
//! JSON-RPC handler for both feature=browser and feature=node.
//! We support running the JSON-RPC server in either native or browser environment.
//! For the native environment, we use jsonrpc_core to handle requests.
//! For the browser environment, we utilize a Simple MessageHandler to process the requests.
use core::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;

use super::server;
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
    dyn Fn(Params, Arc<Processor>) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>>
        + Send
        + Sync,
>;

/// Type of handler function, note that, in browser environment there is no Send and Sync
#[cfg(feature = "browser")]
pub type MethodFnBox =
    Box<dyn Fn(Params, Arc<Processor>) -> Pin<Box<dyn Future<Output = Result<Value>>>>>;

/// This macro is used to transform an asynchronous function item into
/// a boxed function reference that returns a pinned future.
macro_rules! pin {
    ($fn:path) => {
        Box::new(|params, meta| Box::pin($fn(params, meta)))
    };
}

/// The InternalRpcHandler is
/// consistent with the signature type MetaIoHandler in jsonrpc_core,
/// but it is more simpler and does not have Send and Sync bounds.
#[derive(Clone)]
pub struct InternalRpcHandler {
    processor: Arc<Processor>,
    /// Registered Methods
    methods: DashMap<String, Arc<MethodFnBox>>,
}

/// A trait that defines the method handler for handling method requests.
#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
pub trait MethodHandler {
    /// Handles the incoming method request and returns the result.
    /// Please note that the function type here is MethodCall instead of
    /// Request in jsonrpc_core, which means that batch requests are not supported here.
    async fn handle_request(&self, request: MethodCall) -> Result<Output>;
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl MethodHandler for InternalRpcHandler {
    async fn handle_request(&self, request: MethodCall) -> Result<Output> {
        let output: Result<Value> = if let Some(handler) = self.methods.get(&request.method) {
            let ret = handler(request.params, self.processor.clone()).await?;
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

impl InternalRpcHandler {
    /// Create a new instance of message handler
    pub fn new(processor: Arc<Processor>) -> Self {
        let r = Self {
            processor,
            methods: DashMap::new(),
        };

        for (m, f) in internal_methods() {
            r.methods.insert(m.to_string(), Arc::new(f));
        }

        r
    }
}

/// This function will return a list of public functions for all interfaces.
/// If you need to define interfaces separately for the browser or native,
/// you should use cfg to control the conditions.
pub fn internal_methods() -> Vec<(Method, MethodFnBox)> {
    vec![
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

/// This function will return a list of public functions for interfaces
/// can be safely exposed to the browser.
/// If you need to define interfaces separately for the browser or native,
/// you should use cfg to control the conditions.
pub fn external_methods() -> Vec<(Method, MethodFnBox)> {
    vec![
        (Method::AnswerOffer, pin!(server::answer_offer)),
        (Method::NodeInfo, pin!(server::node_info)),
        (Method::NodeDid, pin!(server::node_did)),
    ]
}

/// Implementation for native node
#[cfg(feature = "node")]
pub mod default {
    use super::*;
    use crate::prelude::jsonrpc_core::MetaIoHandler;

    /// Register all internal methods to the jsonrpc handler
    pub async fn register_internal_methods(handler: &mut MetaIoHandler<Arc<Processor>>) {
        for (m, f) in internal_methods() {
            handler.add_method_with_meta(m.as_str(), f);
        }
    }

    /// Register all external methods to the jsonrpc handler
    pub async fn register_external_methods(handler: &mut MetaIoHandler<Arc<Processor>>) {
        for (m, f) in external_methods() {
            handler.add_method_with_meta(m.as_str(), f);
        }
    }
}
