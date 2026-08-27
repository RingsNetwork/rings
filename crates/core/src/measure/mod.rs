//! This module provide the `Measure` struct and its implementations.
//! It is used to assess the reliability of remote peers.
#![deny(missing_docs)]

mod behaviour;
mod counter;
mod quality;

pub use behaviour::BehaviourJudgement;
pub use behaviour::ConnectBehaviour;
pub use behaviour::Measure;
pub use behaviour::MeasureImpl;
pub use behaviour::MessageRecvBehaviour;
pub use behaviour::MessageSendBehaviour;
pub use counter::MeasureCounter;
pub use quality::order_peers_by_quality;
pub use quality::PeerMeasurement;
pub use quality::PeerQuality;
pub use quality::PeerQualityEvidence;
pub use quality::PeerQualityThresholds;

#[cfg(test)]
mod tests;
