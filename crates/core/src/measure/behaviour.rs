use std::sync::Arc;

use async_trait::async_trait;

use crate::dht::Did;

/// Type of Measure, see [Measure].
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub type MeasureImpl = Arc<dyn BehaviourJudgement + Send + Sync>;

/// Type of Measure, see [crate::measure::Measure].
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub type MeasureImpl = Arc<dyn BehaviourJudgement>;

use super::MeasureCounter;
use super::PeerQuality;

/// `Measure` is used to assess the reliability of peers by counting their behaviour.
/// It currently count the number of sent and received messages in a given period (1 hour).
/// The method [Measure::incr] should be called in the proper places.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait Measure {
    /// `incr` increments the counter of the given peer.
    async fn incr(&self, did: Did, counter: MeasureCounter);
    /// `get_count` returns the counter of the given peer.
    async fn get_count(&self, did: Did, counter: MeasureCounter) -> u64;
}

/// `BehaviourJudgement` classifies local evidence about a peer.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait BehaviourJudgement: Measure {
    /// Classify local peer quality for DHT connection scheduling.
    ///
    /// This value is advisory. It orders connection attempts and does not gate
    /// Chord membership, routing, ownership, or storage placement.
    async fn quality(&self, did: Did) -> PeerQuality;

    /// Return the legacy boolean judgement for callers that need a yes/no decision.
    ///
    /// This method is intentionally independent from [Self::quality]. Mapping
    /// the three-valued quality order to a boolean would turn advisory DHT
    /// scheduling evidence into a hidden gating rule.
    async fn good(&self, did: Did) -> bool;
}

/// `ConnectBehaviour` trait offers a default implementation for the `good` method, providing a judgement
/// based on a node's behavior in establishing connections.
/// The "goodness" of a node is measured by comparing disconnection counts against a given threshold.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait ConnectBehaviour<const THRESHOLD: u64>: Measure {
    /// This asynchronous method returns a boolean indicating whether the node identified by `did` has a satisfactory connection behavior.
    async fn good(&self, did: Did) -> bool {
        let conn = self.get_count(did, MeasureCounter::Connect).await;
        let disconn = self.get_count(did, MeasureCounter::Disconnected).await;
        tracing::debug!(
            "[ConnectBehaviour] in threshold: {:}, connect: {:}, disconn: {:}",
            THRESHOLD,
            conn,
            disconn
        );
        disconn < THRESHOLD
    }
}

/// `MessageSendBehaviour` trait provides a default implementation for the `good` method, judging a node's
/// behavior based on its message sending capabilities.
/// The "goodness" of a node is measured by comparing the sent and failed-to-send counts against a given threshold.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait MessageSendBehaviour<const THRESHOLD: u64>: Measure {
    /// This asynchronous method returns a boolean indicating whether the node identified by `did` has a satisfactory message sending behavior.
    async fn good(&self, did: Did) -> bool {
        let failed = self.get_count(did, MeasureCounter::FailedToSend).await;
        failed < THRESHOLD
    }
}

/// `MessageRecvBehaviour` trait provides a default implementation for the `good` method, assessing a node's
/// behavior based on its message receiving capabilities.
/// The "goodness" of a node is measured by comparing the received and failed-to-receive counts against a given threshold.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait MessageRecvBehaviour<const THRESHOLD: u64>: Measure {
    /// This asynchronous method returns a boolean indicating whether the node identified by `did` has a satisfactory message receiving behavior.
    async fn good(&self, did: Did) -> bool {
        let failed = self.get_count(did, MeasureCounter::FailedToReceive).await;
        failed < THRESHOLD
    }
}
