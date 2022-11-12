#![warn(missing_docs)]
//! Backend Message Types.
use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;
use crate::prelude::*;

/// Enum MessageType of BackendMessage.
#[derive(Debug, Clone)]
pub enum MessageType {
    /// unknown
    Unknown = 0,
    /// empty
    Empty,
    /// simple texte
    SimpleText,
    /// http reuqest
    HttpRequest,
    /// http response
    HttpResponse,
}

impl From<&[u8; 2]> for MessageType {
    fn from(v: &[u8; 2]) -> Self {
        Self::from(u16::from_le_bytes(*v))
    }
}

impl From<u16> for MessageType {
    fn from(v: u16) -> Self {
        match v {
            1 => MessageType::Empty,
            2 => MessageType::SimpleText,
            3 => MessageType::HttpRequest,
            4 => MessageType::HttpResponse,
            _ => MessageType::Unknown,
        }
    }
}

impl From<MessageType> for u16 {
    fn from(v: MessageType) -> Self {
        match v {
            MessageType::Unknown => 0,
            MessageType::Empty => 1,
            MessageType::SimpleText => 2,
            MessageType::HttpRequest => 3,
            MessageType::HttpResponse => 4,
        }
    }
}

/// BackendMessage struct for CustomMessage.
/// A backend message body's length at least is 32bytes;
/// - `message_type`: [u8;2]
/// - `extra data`: [u8;30]
/// - `message data`: [u8]
#[derive(Debug, Clone)]
pub struct BackendMessage {
    /// message_type
    pub message_type: MessageType,
    /// extra bytes
    pub extra: [u8; 30],
    /// data body
    pub data: Vec<u8>,
}

impl BackendMessage {
    /// generate new BackendMessage with
    /// - `message_type`
    /// - `data`
    /// extra will be [0u8;30]
    pub fn new(message_type: MessageType, data: &[u8]) -> Self {
        Self {
            message_type,
            extra: [0u8; 30],
            data: data.to_vec(),
        }
    }
}

impl<T> TryFrom<(MessageType, &T)> for BackendMessage
where T: Serialize
{
    type Error = Error;

    fn try_from((message_type, data): (MessageType, &T)) -> std::result::Result<Self, Self::Error> {
        let bytes = bincode::serialize(data).map_err(|_| Error::SerializeError)?;
        Ok(Self::new(message_type, &bytes))
    }
}

impl TryFrom<&[u8]> for BackendMessage {
    type Error = Error;

    #[allow(clippy::ptr_offset_with_cast)]
    fn try_from(value: &[u8]) -> std::result::Result<Self, Self::Error> {
        if value.len() < 32 {
            return Err(Error::NotSupportMessage);
        }
        let (left, right) = arrayref::array_refs![value, 32; ..;];
        let (message_type, _) = arrayref::array_refs![left, 2; ..;];

        Ok(Self::new(message_type.into(), right))
    }
}

impl TryFrom<Vec<u8>> for BackendMessage {
    type Error = Error;

    fn try_from(value: Vec<u8>) -> std::result::Result<Self, Self::Error> {
        BackendMessage::try_from(value.as_slice())
    }
}

impl From<BackendMessage> for Bytes {
    fn from(v: BackendMessage) -> Self {
        let d: Vec<u8> = v.into();
        Self::from(d)
    }
}

impl From<BackendMessage> for Vec<u8> {
    fn from(v: BackendMessage) -> Self {
        let mut data = Vec::new();
        let t: u16 = v.message_type.into();
        data.extend_from_slice(&t.to_le_bytes());
        data.extend_from_slice(&v.extra);
        data.extend_from_slice(&v.data);
        data
    }
}

/// Enum HttpServerRequest with Request, and HttpServerResponse with Response.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpServerMessage {
    /// Request Http Message
    Request(HttpServerRequest),
    /// Response Http Message
    Response(HttpServerResponse),
}

/// HttpServerRequest
/// - `method`: GET, POST, DELETE, PUT, OPTION, HEAD.
/// - `path`: URI and URL
/// - `headers`: HashMap contains like `Context-Type: application/json`.
/// - `body`:  Message chunk split into bytes and send to remote client.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpServerRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Option<Bytes>,
}

/// HttpServerResponse
/// - `status`: Status machine with numbers, like 200, 300, 400, 500.
/// - `headers`: HashMap contains like `Context-Type: application/json`.
/// - `body`:  Message chunk split into bytes and send back to remote client.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpServerResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: Option<Bytes>,
}

/// Timeout in milliseconds.
#[derive(Deserialize, Debug, Serialize, Clone)]
pub struct Timeout(u64);

impl Default for Timeout {
    fn default() -> Self {
        Timeout(30 * 1000)
    }
}

impl From<Timeout> for Duration {
    fn from(val: Timeout) -> Self {
        Duration::from_millis(val.0)
    }
}

impl From<u64> for Timeout {
    fn from(v: u64) -> Self {
        Self(v)
    }
}

/// Message Endpoint trait
#[async_trait::async_trait]
pub trait MessageEndpoint {
    /// handle_message
    async fn handle_message(
        &self,
        handler: &MessageHandler,
        ctx: &MessagePayload<Message>,
        relay: &MessageRelay,
        data: &BackendMessage,
    ) -> Result<()>;
}

fn default_http_request_body() -> Option<Bytes> {
    None
}

/// HttpRequest
/// - `method`: request methods
///    * GET
///    * POST
///    * PUT
///    * DELETE
///    * OPTION
///    * HEAD
///    * TRACE
///    * CONNECT
/// - `url`: http URL
/// - `timeout`: timeout in milliseconds
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpRequest {
    /// method
    pub method: String,
    /// url
    pub url: String,
    /// timeout
    #[serde(default)]
    pub timeout: Timeout,
    /// headers
    pub headers: HashMap<String, String>,
    /// body
    #[serde(default = "default_http_request_body")]
    pub body: Option<Bytes>,
}

impl From<(http::Method, String, Timeout)> for HttpRequest {
    fn from((method, url, timeout): (http::Method, String, Timeout)) -> Self {
        Self {
            method: method.to_string(),
            url,
            timeout,
            headers: HashMap::new(),
            body: None,
        }
    }
}

impl From<(http::Method, &str, u64)> for HttpRequest {
    fn from((method, url, timeout): (http::Method, &str, u64)) -> Self {
        (method, url.to_owned(), Timeout::from(timeout)).into()
    }
}

impl HttpRequest {
    /// new HttpRequest
    /// - `method`
    /// - `url`
    /// - `timeout`
    /// - `headers`
    /// - `body`: optional
    pub fn new<U>(
        method: http::Method,
        url: U,
        timeout: Timeout,
        headers: &[(U, U)],
        body: Option<Bytes>,
    ) -> Self
    where
        U: ToString,
    {
        Self {
            method: method.to_string(),
            url: url.to_string(),
            timeout,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body,
        }
    }

    /// new `GET` HttpRequest
    /// - `method`
    /// - `url`
    /// - `timeout`
    /// - `headers`
    /// - `body`: optional
    pub fn get<U>(url: U, timeout: Timeout, headers: &[(U, U)], body: Option<Bytes>) -> Self
    where U: ToString {
        Self::new(http::Method::GET, url, timeout, headers, body)
    }
}

/// HttpResponse
/// - `status`: Status machine with numbers, like 200, 300, 400, 500.
/// - `body`: Message chunk split bytes and send back to remote clinet.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpResponse {
    /// status
    pub status: u16,
    /// headers
    pub headers: HashMap<String, String>,
    /// body: optional
    pub body: Option<Bytes>,
}

/// send chunk report message
/// - `handler`
/// - `ctx`
/// - `relay`
/// - `data`
pub async fn send_chunk_report_message(
    handler: &MessageHandler,
    ctx: &MessagePayload<Message>,
    relay: &MessageRelay,
    data: &[u8],
) -> Result<()> {
    let mut new_bytes: Vec<u8> = Vec::with_capacity(data.len() + 1);
    new_bytes.push(1);
    new_bytes.extend_from_slice(&[0u8; 3]);
    new_bytes.extend_from_slice(data);

    handler
        .send_report_message(
            Message::custom(&new_bytes, None).map_err(|_| Error::InvalidMessage)?,
            ctx.tx_id,
            relay.clone(),
        )
        .await
        .map_err(Error::SendMessage)?;
    Ok(())
}
