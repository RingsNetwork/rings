//! A JSONRPC response.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::error::Error;
use crate::error::Result;
use crate::prelude::rings_core::inspect::SwarmInspect;

/// Peer contains transport address and state information.
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Peer {
    /// a processor' address
    pub did: String,
    /// a transport protocol using in swarm instance
    pub cid: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageResponse {
    pub tx_id: String,
}

impl From<String> for SendMessageResponse {
    fn from(v: String) -> Self {
        Self { tx_id: v }
    }
}

/// NodeInfo struct
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeInfo {
    /// node version
    pub version: String,
    /// swarm inspect info
    pub swarm: SwarmInspect,
}
