use std::sync::Arc;

use crate::error::{Error, Result};
use bns_core::transports::default::DefaultTransport;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use web3::types::H160;

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Peer {
    pub address: String,
    pub transport_id: String,
}

impl Peer {
    pub fn to_json_vec(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|_| Error::JsonSerializeError)
    }

    pub fn to_json_obj(&self) -> Result<JsonValue> {
        serde_json::to_value(self).map_err(|_| Error::JsonSerializeError)
    }

    pub fn base64_encode(&self) -> Result<String> {
        Ok(base64::encode(self.to_json_vec()?))
    }
}

impl From<(H160, Arc<DefaultTransport>)> for Peer {
    fn from((address, transport): (H160, Arc<DefaultTransport>)) -> Self {
        Self {
            address: address.to_string(),
            transport_id: transport.id.to_string(),
        }
    }
}

impl From<&(H160, Arc<DefaultTransport>)> for Peer {
    fn from((address, transport): &(H160, Arc<DefaultTransport>)) -> Self {
        Self {
            address: address.to_string(),
            transport_id: transport.id.to_string(),
        }
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

    pub fn base64_encode(&self) -> Result<String> {
        Ok(base64::encode(self.to_json_vec()?))
    }
}
