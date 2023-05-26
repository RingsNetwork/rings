#![warn(missing_docs)]
//! Backend Message Types.
use std::collections::HashMap;

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
/// - `message_type`: `[u8;2]`
/// - `extra data`: `[u8;30]`
/// - `message data`: `[u8]`
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// extra will be `[0u8;30]`
    pub fn new(message_type: u16, extra: [u8; 30], data: &[u8]) -> Self {
        Self {
            message_type,
            extra,
            data: data.to_vec(),
        }
    }
}

impl From<(u16, &[u8])> for BackendMessage {
    fn from((message_type, data): (u16, &[u8])) -> Self {
        Self::new(message_type, [0u8; 30], data)
    }
}

impl<T> TryFrom<(MessageType, &T)> for BackendMessage
where T: Serialize
{
    type Error = Error;

    fn try_from((message_type, data): (MessageType, &T)) -> std::result::Result<Self, Self::Error> {
        let bytes = bincode::serialize(data).map_err(|_| Error::EncodeError)?;
        Ok(Self::new(message_type.into(), [0u8; 30], &bytes))
    }
}

impl TryFrom<&[u8]> for BackendMessage {
    type Error = Error;

    #[allow(clippy::ptr_offset_with_cast)]
    fn try_from(value: &[u8]) -> std::result::Result<Self, Self::Error> {
        if value.len() < 32 {
            return Err(Error::InvalidMessage);
        }
        let (left, right) = arrayref::array_refs![value, 32; ..;];
        let (message_type, _) = arrayref::array_refs![left, 2; ..;];

        Ok(Self::new(
            u16::from_le_bytes(*message_type),
            [0u8; 30],
            right,
        ))
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

/// Message Endpoint trait
#[async_trait::async_trait]
pub trait MessageEndpoint {
    /// handle_message
    async fn handle_message(
        &self,
        ctx: &MessagePayload<Message>,
        data: &BackendMessage,
    ) -> Result<Vec<MessageHandlerEvent>>;
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
