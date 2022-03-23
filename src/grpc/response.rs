use std::sync::Arc;

use bns_core::transports::default::DefaultTransport;
use serde::{Deserialize, Serialize};
use web3::types::H160;

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Peer {
    pub address: String,
    pub transport_id: String,
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
