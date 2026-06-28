//! This module provide the `Measure` struct and its implementations.
//! It is used to assess the reliability of remote peers.
#![warn(missing_docs)]
use std::sync::Arc;

use async_trait::async_trait;

use crate::dht::Did;

/// Type of Measure, see [Measure].
#[cfg(not(feature = "wasm"))]
pub type MeasureImpl = Arc<dyn BehaviourJudgement + Send + Sync>;

/// Type of Measure, see [crate::measure::Measure].
#[cfg(feature = "wasm")]
pub type MeasureImpl = Arc<dyn BehaviourJudgement>;

/// The tag of counters in measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeasureCounter {
    /// The number of sent messages.
    Sent,
    /// The number of failed to sent messages.
    FailedToSend,
    /// The number of received messages.
    Received,
    /// The number of failed to receive messages.
    FailedToReceive,
    /// The number of connected.
    Connect,
    /// The number of disconnect.
    Disconnected,
}

/// Local peer-quality class derived from observation counters.
///
/// This value is advisory. It orders DHT connection attempts, but it is not a
/// Chord membership, ownership, or storage-placement proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerQuality {
    /// The peer has positive successful observations and remains below failure limits.
    Healthy,
    /// The local node has no useful recent evidence for this peer.
    Unknown,
    /// The peer reached one or more local failure limits.
    Degraded,
}

impl PeerQuality {
    /// Return the stable connection-priority rank: smaller is tried first.
    pub const fn connection_rank(self) -> u8 {
        match self {
            Self::Healthy => 0,
            Self::Unknown => 1,
            Self::Degraded => 2,
        }
    }
}

/// Failure limits used to classify local peer-quality evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerQualityThresholds {
    disconnected: u64,
    failed_to_send: u64,
    failed_to_receive: u64,
}

impl PeerQualityThresholds {
    /// Create classification thresholds.
    pub const fn new(disconnected: u64, failed_to_send: u64, failed_to_receive: u64) -> Self {
        Self {
            disconnected,
            failed_to_send,
            failed_to_receive,
        }
    }
}

/// Recent local evidence used to classify a peer.
///
/// The counters are local observations only. They do not claim global
/// reputation and are not signed or replicated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerQualityEvidence {
    connected: u64,
    disconnected: u64,
    sent: u64,
    failed_to_send: u64,
    received: u64,
    failed_to_receive: u64,
}

impl PeerQualityEvidence {
    /// Build evidence from explicit counter values.
    pub const fn new(
        connected: u64,
        disconnected: u64,
        sent: u64,
        failed_to_send: u64,
        received: u64,
        failed_to_receive: u64,
    ) -> Self {
        Self {
            connected,
            disconnected,
            sent,
            failed_to_send,
            received,
            failed_to_receive,
        }
    }

    /// Read all counters for `did` from a measurement implementation.
    pub async fn from_measure<M>(measure: &M, did: Did) -> Self
    where M: Measure + ?Sized {
        Self {
            connected: measure.get_count(did, MeasureCounter::Connect).await,
            disconnected: measure.get_count(did, MeasureCounter::Disconnected).await,
            sent: measure.get_count(did, MeasureCounter::Sent).await,
            failed_to_send: measure.get_count(did, MeasureCounter::FailedToSend).await,
            received: measure.get_count(did, MeasureCounter::Received).await,
            failed_to_receive: measure
                .get_count(did, MeasureCounter::FailedToReceive)
                .await,
        }
    }

    /// Classify this evidence under the supplied thresholds.
    pub const fn classify(self, thresholds: PeerQualityThresholds) -> PeerQuality {
        if self.reaches_failure_limit(thresholds) {
            PeerQuality::Degraded
        } else if self.has_positive_observation() {
            PeerQuality::Healthy
        } else {
            PeerQuality::Unknown
        }
    }

    /// Return whether any successful local observation exists.
    pub const fn has_positive_observation(self) -> bool {
        self.connected > 0 || self.sent > 0 || self.received > 0
    }

    /// Return whether any failure counter has reached its configured limit.
    pub const fn reaches_failure_limit(self, thresholds: PeerQualityThresholds) -> bool {
        self.disconnected >= thresholds.disconnected
            || self.failed_to_send >= thresholds.failed_to_send
            || self.failed_to_receive >= thresholds.failed_to_receive
    }
}

/// Order DHT connection candidates by advisory peer quality.
///
/// Invariant: the returned list is a stable permutation of the input candidate
/// sequence. The transformation never inserts or removes a `Did`; it only moves
/// `Healthy` before `Unknown` before `Degraded`.
/// Preservation: because the set of candidates is unchanged, Chord ownership,
/// successor responsibility, and storage placement remain determined only by the
/// DHT transition that produced those candidates.
pub fn order_peers_by_quality(
    candidates: impl IntoIterator<Item = (Did, PeerQuality)>,
) -> Vec<Did> {
    let mut ranked = candidates
        .into_iter()
        .enumerate()
        .map(|(index, (did, quality))| (quality.connection_rank(), index, did))
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(rank, index, _)| (*rank, *index));
    ranked.into_iter().map(|(_, _, did)| did).collect()
}

/// `Measure` is used to assess the reliability of peers by counting their behaviour.
/// It currently count the number of sent and received messages in a given period (1 hour).
/// The method [Measure::incr] should be called in the proper places.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait Measure {
    /// `incr` increments the counter of the given peer.
    async fn incr(&self, did: Did, counter: MeasureCounter);
    /// `get_count` returns the counter of the given peer.
    async fn get_count(&self, did: Did, counter: MeasureCounter) -> u64;
}

/// `BehaviourJudgement` classifies local evidence about a peer.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
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
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
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
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
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
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait MessageRecvBehaviour<const THRESHOLD: u64>: Measure {
    /// This asynchronous method returns a boolean indicating whether the node identified by `did` has a satisfactory message receiving behavior.
    async fn good(&self, did: Did) -> bool {
        let failed = self.get_count(did, MeasureCounter::FailedToReceive).await;
        failed < THRESHOLD
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;

    fn did() -> Did {
        SecretKey::random().address().into()
    }

    #[test]
    fn peer_quality_evidence_classifies_unknown_healthy_and_degraded() {
        let thresholds = PeerQualityThresholds::new(3, 10, 10);
        assert_eq!(
            PeerQualityEvidence::new(0, 0, 0, 0, 0, 0).classify(thresholds),
            PeerQuality::Unknown
        );
        assert_eq!(
            PeerQualityEvidence::new(1, 0, 0, 0, 0, 0).classify(thresholds),
            PeerQuality::Healthy
        );
        assert_eq!(
            PeerQualityEvidence::new(1, 3, 0, 0, 0, 0).classify(thresholds),
            PeerQuality::Degraded
        );
        assert_eq!(
            PeerQualityEvidence::new(1, 0, 0, 10, 0, 0).classify(thresholds),
            PeerQuality::Degraded
        );
        assert_eq!(
            PeerQualityEvidence::new(1, 0, 0, 0, 0, 10).classify(thresholds),
            PeerQuality::Degraded
        );
    }

    #[test]
    fn order_peers_by_quality_is_stable_permutation() {
        let degraded = did();
        let unknown_a = did();
        let healthy = did();
        let unknown_b = did();

        let ordered = order_peers_by_quality([
            (degraded, PeerQuality::Degraded),
            (unknown_a, PeerQuality::Unknown),
            (healthy, PeerQuality::Healthy),
            (unknown_b, PeerQuality::Unknown),
        ]);

        assert_eq!(ordered, vec![healthy, unknown_a, unknown_b, degraded]);
    }
}
