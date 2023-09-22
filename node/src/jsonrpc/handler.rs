#![warn(missing_docs)]
//! JSON-RPC handler for both feature=browser and feature=node.
//! We support running the JSON-RPC server in either native or browser environment.
//! For the native environment, we use jsonrpc_core to handle requests.
//! For the browser environment, we utilize a Simple MessageHandler to process the requests.

use core::future::Future;
use std::pin::Pin;

#[cfg(feature = "browser")]
pub use self::browser::build_handler;
#[cfg(feature = "browser")]
pub use self::browser::HandlerType;
#[cfg(feature = "node")]
pub use self::default::build_handler;
#[cfg(feature = "browser")]
pub use self::browser::into_message_handler;
#[cfg(feature = "node")]
pub use self::default::into_message_handler;
#[cfg(feature = "node")]
pub use self::default::HandlerType;
use super::server;
use super::server::RpcMeta;
use crate::prelude::jsonrpc_core::Params;
use crate::prelude::jsonrpc_core::Result;
use crate::prelude::jsonrpc_core::Value;
use crate::prelude::rings_rpc::method::Method;

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
        (
            Method::SendHttpRequestMessage,
            pin!(server::send_http_request_message),
        ),
        (
            Method::SendSimpleText,
            pin!(server::send_simple_text_message),
        ),
        (Method::SendCustomMessage, pin!(server::send_custom_message)),
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
        #[cfg(feature = "node")]
        (Method::PollMessage, pin!(default::poll_backend_message)),
    ]
}

/// Implementation for browser
#[cfg(feature = "browser")]
pub mod browser {
    //! This module provides two main facilities:
    //! [MessageHandler]: It is used to serve JSON-RPC in the browser and
    //! handle incoming request requests from the browser.
    //! [build_handler]: It will be used to register and initialize interfaces.
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::prelude::jsonrpc_core::types::error::Error;
    use crate::prelude::jsonrpc_core::types::error::ErrorCode;
    use crate::prelude::jsonrpc_core::types::request::MethodCall;
    use crate::prelude::jsonrpc_core::types::response::Output;
    use crate::processor::Processor;

    /// Type of Messagehandler
    pub type HandlerType = MessageHandler<server::RpcMeta>;

    /// The signature type of MessageHandler is
    /// consistent with the signature type MetaIoHandler in jsonrpc_core,
    /// but it is more simpler and does not have Send and Sync bounds.
    /// Here, the type T is likely to be RpcMeta indefinitely.
    #[derive(Clone)]
    pub struct MessageHandler<T: Clone> {
        /// MetaData for jsonrpc
        meta: T,
        /// Registered Methods
        methods: HashMap<String, Arc<MethodFnBox>>,
    }

    /// Map from Arc<Processor> to MessageHandler<server::RpcMeta>
    /// The value of is_auth of RpcmMet is set to true
    pub async fn into_message_handler(processor: Arc<Processor>) -> HandlerType {
        let meta: server::RpcMeta = processor.into();
        HandlerType::new(meta)
    }

    /// This trait defines the register function for method registration.
    pub trait MethodRegister {
        /// Registers a method with the given name and function.
        fn register(&mut self, name: &str, func: MethodFnBox);
    }

    /// A trait that defines the method handler for handling method requests.
    #[cfg_attr(feature = "browser", async_trait(?Send))]
    pub trait MethodHandler {
        /// Handles the incoming method request and returns the result.
        /// Please note that the function type here is MethodCall instead of
        /// Request in jsonrpc_core, which means that batch requests are not supported here.
        async fn handle_request(&self, request: MethodCall) -> Result<Output>;
    }

    impl MessageHandler<server::RpcMeta> {
        /// Create a new instance of message handler
        pub fn new(meta: server::RpcMeta) -> Self {
            Self {
                meta,
                methods: HashMap::new(),
            }
        }
    }

    impl<T: Clone> MethodRegister for MessageHandler<T> {
        fn register(&mut self, name: &str, func: MethodFnBox) {
            self.methods.insert(name.to_string(), Arc::new(func));
        }
    }

    #[cfg_attr(feature = "browser", async_trait(?Send))]
    #[cfg_attr(not(feature = "browser"), async_trait)]
    impl MethodHandler for MessageHandler<server::RpcMeta> {
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

    /// Build handler add method with metadata.
    pub async fn build_handler(handler: &mut MessageHandler<server::RpcMeta>) {
        for m in methods() {
            handler.register(m.0.as_str(), m.1);
        }
    }
}

/// Implementation for native node
#[cfg(feature = "node")]
pub mod default {
    use super::*;
    use crate::error::Error as ServerError;
    use crate::prelude::jsonrpc_core::Error;
    use crate::prelude::jsonrpc_core::MetaIoHandler as MessageHandler;
    use crate::prelude::rings_rpc::response::CustomBackendMessage;
    use std::sync::Arc;
    use crate::processor::Processor;

    /// Type of Messagehandler
    pub type HandlerType = MessageHandler<server::RpcMeta>;

    /// Build handler add method with metadata.
    pub async fn build_handler(handler: &mut MessageHandler<server::RpcMeta>) {
        for m in methods() {
            handler.add_method_with_meta(m.0.as_str(), m.1);
        }
    }

    /// Map from Arc<Processor> to MessageHandler<server::RpcMeta>
    /// The value of is_auth of RpcmMet is set to true
    pub async fn into_message_handler(_processor: Arc<Processor>) -> HandlerType {
	let mut jsonrpc_handler: MessageHandler<RpcMeta> = MessageHandler::default();
	build_handler(&mut jsonrpc_handler).await;
	jsonrpc_handler
    }

    /// This function is used to handle custom messages for the backend.
    pub async fn poll_backend_message(params: Params, meta: server::RpcMeta) -> Result<Value> {
        let receiver = if let Some(value) = meta.receiver {
            value
        } else {
            return Ok(serde_json::Value::Null);
        };

        let params: Vec<serde_json::Value> = params.parse()?;
        let wait_recv = params
            .get(0)
            .map(|v| v.as_bool().unwrap_or(false))
            .unwrap_or(false);
        let message = if wait_recv {
            let mut recv = receiver.lock().await;
            recv.recv().await.ok()
        } else {
            let mut recv = receiver.lock().await;
            recv.try_recv().ok()
        };

        let message = if let Some(msg) = message {
            serde_json::to_value(CustomBackendMessage::from(msg))
                .map_err(|_| Error::from(ServerError::EncodeError))?
        } else {
            serde_json::Value::Null
        };
        Ok(serde_json::json!({
            "message": message,
        }))
    }
}
