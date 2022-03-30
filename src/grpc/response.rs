use std::sync::Arc;

use bns_core::transports::default::DefaultTransport;
use serde::{Deserialize, Serialize};
use web3::types::H160;

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Peer {
    pub address: String,
    pub transport_id: String,
}

impl Peer {
    pub fn to_json_vec(&self) -> anyhow::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|e| anyhow::anyhow!("json_err: {}", e))
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
pub struct TransportAndHsInfo {
    pub transport_id: String,
    pub handshake_info: String,
}

impl TransportAndHsInfo {
    pub fn new(transport_id: &str, handshake_info: &str) -> Self {
        Self {
            transport_id: transport_id.to_owned(),
            handshake_info: handshake_info.to_owned(),
        }
    }
    pub fn to_json_vec(&self) -> anyhow::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|e| e.into())
    }
}
