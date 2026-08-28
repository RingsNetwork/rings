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

impl PeerMeasurement {
    /// Read counter-only compatibility evidence for `did`.
    ///
    /// Returns `None` when every counter is zero. Counter-only implementations
    /// cannot provide byte credits and therefore project neutral credit.
    pub async fn from_measure<M>(measure: &M, did: Did) -> Option<Self>
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
        if evidence.is_unobserved() {
            return None;
        }
        let thresholds = PeerQualityThresholds::new(u64::MAX, u64::MAX, u64::MAX);
        Some(Self {
            did,
            evidence,
            credit: None,
            credit_score: rings_measure::CreditScore::NEUTRAL,
            quality: evidence.classify(thresholds),
        })
    }

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

/// Stably order DHT candidates by advisory reliability.
///
/// Invariant: the output is a stable permutation of the input candidate set.
pub fn order_peers_by_quality(
    candidates: impl IntoIterator<Item = (Did, PeerQuality)>,
) -> Vec<Did> {
    rings_measure::order_peers_by_reliability(candidates)
}
