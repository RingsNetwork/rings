#![warn(missing_docs)]
//! This module defines type and type alias related to Swarm.
use async_trait::async_trait;
use rings_transport::core::transport::ConnectionInterface;

use crate::dht::Did;
use crate::dht::LiveDid;
use crate::measure::BehaviourJudgement;
use crate::swarm::Swarm;
use crate::types::Connection;

/// Type of Measure, see [Measure].
#[cfg(not(feature = "wasm"))]
pub type MeasureImpl = Box<dyn BehaviourJudgement + Send + Sync>;

/// Type of Measure, see [Measure].
#[cfg(feature = "wasm")]
pub type MeasureImpl = Box<dyn BehaviourJudgement>;

/// WrappedDid is a DID wrapped by Swarm and bound to a Connection,
/// which enables checking whether the WrappedDid is live or not.
#[derive(Clone)]
pub struct WrappedDid {
    did: Did,
    connection: Option<Connection>,
}

impl WrappedDid {
    /// Creates a new WrappedDid using the provided Swarm instance and DID.
    pub fn new(swarm: &Swarm, did: Did) -> Self {
        // Try to get the Connection for the provided DID from the Swarm.
        match swarm.backend.connection(did) {
            Some(c) => Self {
                did,
                connection: Some(c),
            },
            None => Self {
                did,
                connection: None,
            },
        }
    }
}

impl From<WrappedDid> for Did {
    /// Implements the From trait to allow conversion from WrappedDid to Did.
    fn from(node: WrappedDid) -> Did {
        node.did
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl LiveDid for WrappedDid {
    /// If the Transport is present and connected, returns true.
    async fn live(&self) -> bool {
        match &self.connection {
            Some(c) => c.is_connected().await,
            None => false,
        }
    }
}
