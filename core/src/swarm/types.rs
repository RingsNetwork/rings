#![warn(missing_docs)]
//! This module defines type and type alias related to Swarm.
use async_trait::async_trait;
#[cfg(not(feature = "wasm"))]
use rings_transport::connections::WebrtcConnection as Connection;
use rings_transport::core::transport::SharedConnection;
use rings_transport::core::transport::SharedTransport;

use crate::dht::Did;
use crate::dht::LiveDid;
use crate::measure::BehaviourJudgement;
use crate::swarm::Swarm;

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
        match swarm.transport.get_connection(&did.to_string()) {
            Ok(c) => Self {
                did,
                // If the Transport is found, create a arc reference to it and store it in the WrappedDid.
                // TODO: Should use weak reference
                connection: Some(c),
            },
            Err(_) => Self {
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
