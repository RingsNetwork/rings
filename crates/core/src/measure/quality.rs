use super::Measure;
use super::MeasureCounter;
use crate::dht::Did;

/// Local peer-quality class derived from recent reliability evidence.
pub type PeerQuality = rings_measure::ReliabilityClass;

/// Failure limits used to classify recent local reliability evidence.
pub type PeerQualityThresholds = rings_measure::ReliabilityThresholds;

/// Recent local logical transport outcomes.
pub type PeerQualityEvidence = rings_measure::ReliabilityEvidence;

/// Projected local measurement for one Rings peer.
///
/// Reliability remains advisory DHT ordering evidence. Credit is a separate
/// local resource-priority relation and is never a Chord membership proof.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeerMeasurement {
    /// Peer DID described by this projection.
    pub did: Did,
    /// Recent logical transport evidence.
    pub evidence: PeerQualityEvidence,
    /// Persistent useful-byte totals when the implementation supports credits.
    pub credit: Option<rings_measure::CreditRecord>,
    /// Local aMule-compatible resource-priority multiplier.
    pub credit_score: rings_measure::CreditScore,
    /// Advisory reliability class derived at query time.
    pub quality: PeerQuality,
}

/// One bounded page of local peer measurements in DID order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PeerMeasurementPage {
    /// Successfully projected measurements in the scanned page.
    pub measurements: Vec<PeerMeasurement>,
    /// Exclusive DID cursor for the next page, absent at the end.
    pub next_cursor: Option<Did>,
}

impl PeerMeasurement {
    /// Convert a pure generic ledger projection into the Rings DTO boundary.
    pub fn from_projected(projected: rings_measure::PeerMeasurement<Did>) -> Self {
        Self {
            did: projected.peer,
            evidence: projected.reliability,
            credit: Some(projected.credit),
            credit_score: projected.credit_score,
            quality: projected.reliability_class,
        }
    }
}

/// Read policy-free evidence from a counter-only compatibility implementation.
///
/// Classification remains the caller's responsibility; this avoids encoding
/// the absence of a policy as sentinel failure thresholds.
pub async fn peer_evidence_from_counters<M>(measure: &M, did: Did) -> Option<PeerQualityEvidence>
where M: Measure + ?Sized {
    let evidence = PeerQualityEvidence {
        connected: measure.get_count(did, MeasureCounter::Connect).await,
        disconnected: measure.get_count(did, MeasureCounter::Disconnected).await,
        sent: measure.get_count(did, MeasureCounter::Sent).await,
        failed_to_send: measure.get_count(did, MeasureCounter::FailedToSend).await,
        received: measure.get_count(did, MeasureCounter::Received).await,
        failed_to_receive: measure
            .get_count(did, MeasureCounter::FailedToReceive)
            .await,
    };
    (!evidence.is_unobserved()).then_some(evidence)
}

/// Stably order DHT candidates by advisory reliability.
///
/// Invariant: the output is a stable permutation of the input candidate set.
pub fn order_peers_by_quality(
    candidates: impl IntoIterator<Item = (Did, PeerQuality)>,
) -> Vec<Did> {
    rings_measure::order_peers_by_reliability(candidates)
}
