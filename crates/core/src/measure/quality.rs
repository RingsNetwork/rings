use super::Measure;
use super::MeasureCounter;
use crate::dht::Did;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerQualityEvidence {
    /// Successful connection observations.
    pub connected: u64,
    /// Disconnection observations.
    pub disconnected: u64,
    /// Successfully sent messages.
    pub sent: u64,
    /// Messages that failed before successful send.
    pub failed_to_send: u64,
    /// Successfully received and verified messages.
    pub received: u64,
    /// Messages that failed decode or verification.
    pub failed_to_receive: u64,
}

/// Local measurement counters for one peer.
///
/// These counters are local observations only. They are not signed, replicated,
/// or global reputation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerMeasurement {
    /// Peer DID these counters describe.
    pub did: Did,
    /// Local evidence counters for this peer.
    pub evidence: PeerQualityEvidence,
}

impl PeerMeasurement {
    /// Read counters for `did` from a measurement implementation.
    ///
    /// Returns `None` when no counter has ever been recorded for `did`; absence
    /// is distinct from an observed peer with non-zero counters.
    pub async fn from_measure<M>(measure: &M, did: Did) -> Option<Self>
    where M: Measure + ?Sized {
        let evidence = PeerQualityEvidence::from_measure(measure, did).await;
        if evidence.is_unobserved() {
            return None;
        }

        Some(Self { did, evidence })
    }
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

    /// Return whether this evidence contains no observed counter.
    pub const fn is_unobserved(self) -> bool {
        self.connected == 0
            && self.disconnected == 0
            && self.sent == 0
            && self.failed_to_send == 0
            && self.received == 0
            && self.failed_to_receive == 0
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
