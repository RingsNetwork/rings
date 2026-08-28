use std::sync::Arc;

use async_trait::async_trait;

use crate::dht::Did;

/// Type of Measure, see [Measure].
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub type MeasureImpl = Arc<dyn BehaviourJudgement + Send + Sync>;

/// Type of Measure, see [crate::measure::Measure].
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub type MeasureImpl = Arc<dyn BehaviourJudgement>;

use rings_measure::ApplyOutcome;
use rings_measure::MeasureError;

use super::MeasureCounter;
use super::MeasurementEvent;
use super::PeerMeasurement;
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

    /// Record one authenticated logical transport event.
    ///
    /// Implementations backed by [`rings_measure::MeasurementLedger`] should
    /// override this method so useful-byte credits are retained. The default is
    /// a compatibility bridge for counter-only test implementations.
    async fn record(
        &self,
        did: Did,
        event: MeasurementEvent,
    ) -> Result<ApplyOutcome, MeasureError> {
        let counter = match event {
            MeasurementEvent::Connected => MeasureCounter::Connect,
            MeasurementEvent::Disconnected => MeasureCounter::Disconnected,
            MeasurementEvent::Sent { .. } => MeasureCounter::Sent,
            MeasurementEvent::FailedToSend => MeasureCounter::FailedToSend,
            MeasurementEvent::Received { .. } => MeasureCounter::Received,
            MeasurementEvent::FailedToReceive => MeasureCounter::FailedToReceive,
        };
        self.incr(did, counter).await;
        Ok(ApplyOutcome::Applied)
    }

    /// Return the projected local measurement for one peer.
    async fn peer_measurement(&self, did: Did) -> Result<Option<PeerMeasurement>, MeasureError> {
        Ok(PeerMeasurement::from_measure(self, did).await)
    }

    /// Return every retained local peer measurement.
    ///
    /// Counter-only compatibility implementations cannot enumerate their key
    /// space and therefore return an empty vector by default.
    async fn peer_measurements(&self) -> Result<Vec<PeerMeasurement>, MeasureError> {
        Ok(Vec::new())
    }
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
