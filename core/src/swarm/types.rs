#![warn(missing_docs)]
//! This module defines type and type alias related to Swarm.
use std::sync::Arc;
use std::sync::Weak;

use async_trait::async_trait;

use crate::dht::Did;
use crate::dht::LiveDid;
use crate::measure::BehaviourJudgement;
use crate::swarm::Swarm;
use crate::transports::manager::TransportManager;
use crate::transports::Transport;
use crate::types::ice_transport::IceTransportInterface;

/// Type of Measure, see [Measure].
#[cfg(not(feature = "wasm"))]
pub type MeasureImpl = Box<dyn BehaviourJudgement + Send + Sync>;

/// Type of Measure, see [Measure].
#[cfg(feature = "wasm")]
pub type MeasureImpl = Box<dyn BehaviourJudgement>;

/// WrappedDid is a DID wrapped by Swarm and bound to a weak reference of a Transport,
/// which enables checking whether the WrappedDid is live or not.
#[derive(Clone)]
pub struct WrappedDid {
    did: Did,
    transport: Option<Weak<Transport>>,
}

impl WrappedDid {
    /// Creates a new WrappedDid using the provided Swarm instance and DID.
    pub fn new(swarm: &Swarm, did: Did) -> Self {
        // Try to get the Transport for the provided DID from the Swarm.
        match swarm.get_transport(did) {
            Some(t) => Self {
                did,
                // If the Transport is found, create a weak reference to it and store it in the WrappedDid.
                transport: Some(Arc::downgrade(&t)),
            },
            None => Self {
                did,
                transport: None,
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
    /// Implements the LiveDid trait for WrappedDid, which checks if the DID is live or not.
    /// If the Transport is not present, has been dropped, or is not connected, returns false.
    /// If the Transport is present and connected, returns true.
    async fn live(&self) -> bool {
        match &self.transport {
            Some(t) => {
                if let Some(transport) = t.upgrade() {
                    // If the weak reference can be upgraded to a strong reference, check if it's connected.
                    transport.is_connected().await
                } else {
                    // If the weak reference cannot be upgraded,
                    // the Transport has been dropped, so return false.
                    false
                }
            }
            None => false,
        }
    }
}
