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
    /// http request
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
    pub message_type: u16,
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
    pub fn new(message_type: u16, data: &[u8]) -> Self {
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
        Ok(Self::new(message_type.into(), &bytes))
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

        Ok(Self::new(u16::from_le_bytes(*message_type), right))
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
        let t: u16 = v.message_type;
        data.extend_from_slice(&t.to_le_bytes());
        data.extend_from_slice(&v.extra);
        data.extend_from_slice(&v.data);
        data
    }
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
/// - `path`: hidden service path
/// - `timeout`: timeout in milliseconds
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpRequest {
    /// service name
    pub name: String,
    /// method
    pub method: String,
    /// url
    pub path: String,
    /// timeout
    #[serde(default)]
    pub timeout: Timeout,
    /// headers
    pub headers: HashMap<String, String>,
    /// body
    #[serde(default = "default_http_request_body")]
    pub body: Option<Bytes>,
}

impl From<(String, http::Method, String, Timeout)> for HttpRequest {
    fn from((name, method, path, timeout): (String, http::Method, String, Timeout)) -> Self {
        Self {
            name,
            method: method.to_string(),
            path,
            timeout,
            headers: HashMap::new(),
            body: None,
        }
    }
}

impl From<(&str, http::Method, &str, u64)> for HttpRequest {
    fn from((name, method, url, timeout): (&str, http::Method, &str, u64)) -> Self {
        (
            name.to_owned(),
            method,
            url.to_owned(),
            Timeout::from(timeout),
        )
            .into()
    }
}

impl HttpRequest {
    /// new HttpRequest
    /// - `name`
    /// - `method`
    /// - `url`
    /// - `timeout`
    /// - `headers`
    /// - `body`: optional
    pub fn new<U>(
        name: U,
        method: http::Method,
        path: U,
        timeout: Timeout,
        headers: &[(U, U)],
        body: Option<Bytes>,
    ) -> Self
    where
        U: ToString,
    {
        Self {
            name: name.to_string(),
            method: method.to_string(),
            path: path.to_string(),
            timeout,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body,
        }
    }

    /// new `GET` HttpRequest
    /// - `name`
    /// - `method`
    /// - `url`
    /// - `timeout`
    /// - `headers`
    /// - `body`: optional
    pub fn get<U>(
        name: U,
        url: U,
        timeout: Timeout,
        headers: &[(U, U)],
        body: Option<Bytes>,
    ) -> Self
    where
        U: ToString,
    {
        Self::new(name, http::Method::GET, url, timeout, headers, body)
    }
}

/// HttpResponse
/// - `status`: Status machine with numbers, like 200, 300, 400, 500.
/// - `body`: Message chunk split bytes and send back to remote client.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpResponse {
    /// status
    pub status: u16,
    /// headers
    pub headers: HashMap<String, String>,
    /// body: optional
    pub body: Option<Bytes>,
}
