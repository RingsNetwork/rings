use super::request::{parse_response, RequestBuilder};
use crate::prelude::reqwest::Client as HttpClient;
use jsonrpc_core::{Error, Params, Value};
use std::sync::Arc;

#[derive(Clone)]
pub struct SimpleClient {
    client: Arc<HttpClient>,
    url: String,
}

impl SimpleClient {
    pub fn new(client: Arc<HttpClient>, url: &str) -> Self {
        Self {
            client,
            url: url.to_owned(),
        }
    }

    pub fn new_with_url(url: &str) -> Self {
        Self {
            client: Arc::new(HttpClient::default()),
            url: url.to_string(),
        }
    }

    pub async fn call_method(&self, method: &str, params: Params) -> RpcResult<Value> {
        let msg = CallMessage {
            method: method.into(),
            params,
        };
        self.do_request(&RpcMessage::Call(msg)).await
    }

    pub async fn notify(&self, method: &str, params: Params) -> RpcResult<()> {
        let msg = NotifyMessage {
            method: method.into(),
            params,
        };
        self.do_request(&RpcMessage::Notify(msg)).await?;
        Ok(())
    }

    async fn do_request(&self, msg: &RpcMessage) -> RpcResult<Value> {
        let mut request_builder = RequestBuilder::new();
        let request = match msg {
            RpcMessage::Call(call) => request_builder.call_request(call).1,
            RpcMessage::Notify(notify) => request_builder.notification(notify),
            RpcMessage::Subscribe(_) => {
                return Err(RpcError::Client(
                    "Unsupported `RpcMessage` type `Subscribe`.".to_owned(),
                ));
            }
        };

        let resp = self
            .client
            .post(self.url.as_str())
            .header(
                http::header::CONTENT_TYPE,
                http::header::HeaderValue::from_static("application/json"),
            )
            .header(
                http::header::ACCEPT,
                http::header::HeaderValue::from_static("application/json"),
            )
            .body(request)
            .send()
            .await
            .map_err(|e| RpcError::Client(e.to_string()))?;
        let resp = resp
            .error_for_status()
            .map_err(|e| RpcError::Client(e.to_string()))?;
        let resp = resp
            .bytes()
            .await
            .map_err(|e| RpcError::ParseError(e.to_string(), Box::new(e)))?;
        let resp_str = String::from_utf8_lossy(&resp).into_owned();
        parse_response(&resp_str)
            .map_err(|e| RpcError::ParseError(e.to_string(), Box::new(e)))?
            .1
    }
}

/// The errors returned by the client.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// An error returned by the server.
    #[error("Server returned rpc error {0}")]
    JsonRpcError(Error),
    /// Failure to parse server response.
    #[error("Failed to parse server response as {0}: {1}")]
    ParseError(String, Box<dyn std::error::Error + Send>),
    /// Request timed out.
    #[error("Request timed out")]
    Timeout,
    /// A general client error.
    #[error("Client error: {0}")]
    Client(String),
    /// Not rpc specific errors.
    #[error("{0}")]
    Other(Box<dyn std::error::Error + Send>),
}

// impl std::error::Error for RpcError {
//     fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
//         match *self {
//             Self::JsonRpcError(ref e) => Some(e),
//             Self::ParseError(_, ref e) => Some(&**e),
//             Self::Other(ref e) => Some(&**e),
//             _ => None,
//         }
//     }
// }

impl From<Error> for RpcError {
    fn from(error: Error) -> Self {
        RpcError::JsonRpcError(error)
    }
}

/// A result returned by the client.
pub type RpcResult<T> = Result<T, RpcError>;

/// An RPC call message.
pub struct CallMessage {
    /// The RPC method name.
    pub method: String,
    /// The RPC method parameters.
    pub params: Params,
}

/// An RPC notification.
pub struct NotifyMessage {
    /// The RPC method name.
    pub method: String,
    /// The RPC method paramters.
    pub params: Params,
}

/// An RPC subscription.
pub struct Subscription {
    /// The subscribe method name.
    pub subscribe: String,
    /// The subscribe method parameters.
    pub subscribe_params: Params,
    /// The name of the notification.
    pub notification: String,
    /// The unsubscribe method name.
    pub unsubscribe: String,
}

/// An RPC subscribe message.
pub struct SubscribeMessage {
    /// The subscription to subscribe to.
    pub subscription: Subscription,
}

/// A message sent to the `RpcClient`.
pub enum RpcMessage {
    /// Make an RPC call.
    Call(CallMessage),
    /// Send a notification.
    Notify(NotifyMessage),
    /// Subscribe to a notification.
    Subscribe(SubscribeMessage),
}

impl From<CallMessage> for RpcMessage {
    fn from(msg: CallMessage) -> Self {
        RpcMessage::Call(msg)
    }
}

impl From<NotifyMessage> for RpcMessage {
    fn from(msg: NotifyMessage) -> Self {
        RpcMessage::Notify(msg)
    }
}

impl From<SubscribeMessage> for RpcMessage {
    fn from(msg: SubscribeMessage) -> Self {
        RpcMessage::Subscribe(msg)
    }
}
