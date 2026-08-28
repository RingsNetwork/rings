//! Runtime boundary for pure local peer measurement algorithms.
#![deny(missing_docs)]

mod behaviour;
mod counter;
mod quality;

pub use behaviour::BehaviourJudgement;
pub use behaviour::Measure;
pub use behaviour::MeasureImpl;
pub use counter::MeasureCounter;
pub use quality::order_peers_by_quality;
pub use quality::peer_evidence_from_counters;
pub use quality::PeerMeasurement;
pub use quality::PeerMeasurementPage;
pub use quality::PeerQuality;
pub use quality::PeerQualityEvidence;
pub use quality::PeerQualityThresholds;
pub use rings_measure::ApplyOutcome;
pub use rings_measure::Authentication;
pub use rings_measure::MeasureError;
pub use rings_measure::MeasurementBatch;
pub use rings_measure::MeasurementEvent;

#[cfg(test)]
mod test_measure;
