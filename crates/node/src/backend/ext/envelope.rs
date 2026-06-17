#![warn(missing_docs)]
//! Wire envelope — the namespaced message carried over the P2P transport.

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;

/// Namespaced message envelope carried over the P2P transport (bincode), in place of
/// the old closed `BackendMessage` enum. `payload` is opaque to the core.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    /// Protocol namespace this payload belongs to.
    pub namespace: String,
    /// Opaque protocol payload; the inner codec is the protocol's own business.
    pub payload: Bytes,
}

impl Envelope {
    /// Build an envelope.
    pub fn new(namespace: impl Into<String>, payload: Bytes) -> Self {
        Self {
            namespace: namespace.into(),
            payload,
        }
    }

    /// Encode for the P2P transport. `encode : Envelope → [u8]`.
    pub fn encode(&self) -> Result<Vec<u8>> {
        bincode::serialize(self).map_err(|_| Error::EncodeError)
    }

    /// Decode from the P2P transport. `decode : [u8] ⇀ Envelope` (partial).
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bincode::deserialize(bytes).map_err(|_| Error::DecodeError)
    }
}
