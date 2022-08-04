use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::error::Error;
use crate::error::Result;
use crate::prelude::rings_core::message::Encoded;
use crate::prelude::rings_core::prelude::web3::contract::tokens::Tokenizable;
use crate::prelude::rings_core::prelude::web3::types::Address;
use crate::prelude::rings_core::transports::Transport;
use crate::processor;

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Peer {
    pub address: String,
    pub transport_id: String,
    pub state: String,
}

impl Peer {
    pub fn to_json_vec(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|_| Error::JsonSerializeError)
    }

    pub fn to_json_obj(&self) -> Result<JsonValue> {
        serde_json::to_value(self).map_err(|_| Error::JsonSerializeError)
    }

    #[cfg(feature = "client")]
    pub fn base64_encode(&self) -> Result<String> {
        Ok(base64::encode(self.to_json_vec()?))
    }
}

impl From<(Address, &Arc<Transport>, Option<String>)> for Peer {
    fn from((address, transport, state): (Address, &Arc<Transport>, Option<String>)) -> Self {
        Self {
            address: address.into_token().to_string(),
            transport_id: transport.id.to_string(),
            state: state.unwrap_or_else(|| "Unknown".to_owned()),
        }
    }
}

impl From<(&processor::Peer, Option<String>)> for Peer {
    fn from((p, state): (&processor::Peer, Option<String>)) -> Self {
        Self {
            address: p.address.clone().into_token().to_string(),
            transport_id: p.transport.id.to_string(),
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

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct TransportAndIce {
    pub transport_id: String,
    pub ice: String,
}

impl TransportAndIce {
    pub fn new(transport_id: &str, ice: &str) -> Self {
        Self {
            transport_id: transport_id.to_owned(),
            ice: ice.to_owned(),
        }
    }

    pub fn to_json_vec(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|_| Error::JsonSerializeError)
    }

    pub fn to_json_obj(&self) -> Result<JsonValue> {
        serde_json::to_value(self).map_err(|_| Error::JsonSerializeError)
    }

    #[cfg(feature = "client")]
    pub fn base64_encode(&self) -> Result<String> {
        Ok(base64::encode(self.to_json_vec()?))
    }
}

impl From<(Arc<Transport>, Encoded)> for TransportAndIce {
    fn from((transport, handshake_info): (Arc<Transport>, Encoded)) -> Self {
        Self {
            transport_id: transport.id.to_string(),
            ice: handshake_info.to_string(),
        }
    }
}
