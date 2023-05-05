//! A JSONRPC response.
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::error::Error;
use crate::error::Result;
use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_core::transports::Transport;

/// Peer contains transport address and state information.
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Peer {
    /// a processor' address
    pub did: String,
    /// a transport protocol using in swarm instance
    pub transport_id: String,
    /// transport ice connection state
    pub state: String,
}

impl Peer {
    pub fn to_json_vec(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|_| Error::EncodeError)
    }

    pub fn to_json_obj(&self) -> Result<JsonValue> {
        serde_json::to_value(self).map_err(|_| Error::EncodeError)
    }
}

impl From<(Did, &Arc<Transport>, Option<String>)> for Peer {
    fn from((did, transport, state): (Did, &Arc<Transport>, Option<String>)) -> Self {
        Self {
            did: did.to_string(),
            transport_id: transport.id.to_string(),
            state: state.unwrap_or_else(|| "Unknown".to_owned()),
        }
    }
}

/// Base Transport Info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportInfo {
    pub transport_id: String,
    pub state: String,
}

impl TransportInfo {
    pub fn new(transport_id: String, state: Option<String>) -> Self {
        Self {
            transport_id,
            state: state.unwrap_or_else(|| "Unknown".to_owned()),
        }
    }
}

impl From<(&Arc<Transport>, Option<String>)> for TransportInfo {
    fn from((transport, state): (&Arc<Transport>, Option<String>)) -> Self {
        Self::new(transport.id.to_string(), state)
    }
}

#[derive(Deserialize, Serialize, Clone)]
pub struct BaseResponse<T> {
    method: String,
    result: T,
}

impl<T> BaseResponse<T>
where T: DeserializeOwned + Serialize + Clone
{
    pub fn new(method: String, result: T) -> Self {
        Self { method, result }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CustomBackendMessage {
    message_type: u16,
    data: String,
}

impl From<(u16, String)> for CustomBackendMessage {
    fn from((message_type, data): (u16, String)) -> Self {
        Self { message_type, data }
    }
}
