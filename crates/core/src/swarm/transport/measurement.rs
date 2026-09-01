use std::num::NonZeroUsize;

use super::delivery::record_measurement;
use super::PendingConnectionAttempt;
use super::SwarmTransport;
use crate::dht::Did;
use crate::measure::order_peers_by_quality;
use crate::measure::Authentication;
use crate::measure::MeasurementEvent;
use crate::measure::PeerMeasurement;
use crate::measure::PeerMeasurementPage;
use crate::measure::PeerQuality;

impl SwarmTransport {
    pub(super) async fn record_peer_measurement(
        &self,
        peer: Did,
        authentication: Authentication,
        event: MeasurementEvent,
    ) {
        record_measurement(self.measure.clone(), peer, authentication, event).await;
    }

    /// Record that a payload from `peer` was accepted and verified by the swarm.
    pub(crate) async fn record_peer_message_received(
        &self,
        attempt: PendingConnectionAttempt,
        authentication: Authentication,
        useful_bytes: u64,
    ) {
        if matches!(authentication, Authentication::Authenticated) {
            self.mark_peer_liveness_inbound(attempt);
        }
        self.record_peer_measurement(attempt.peer, authentication, MeasurementEvent::Received {
            useful_bytes,
        })
        .await;
    }

    /// Record that a payload from `peer` could not be decoded or verified.
    pub(crate) async fn record_peer_message_receive_failed(
        &self,
        peer: Did,
        authentication: Authentication,
    ) {
        self.record_peer_measurement(peer, authentication, MeasurementEvent::FailedToReceive)
            .await;
    }

    /// Record that an outbound payload to `peer` failed before delivery.
    pub(crate) async fn record_peer_message_send_failed(
        &self,
        peer: Did,
        authentication: Authentication,
    ) {
        self.record_peer_measurement(peer, authentication, MeasurementEvent::FailedToSend)
            .await;
    }

    /// Return this node's local quality judgement for `peer`.
    pub(crate) async fn peer_quality(&self, peer: Did) -> PeerQuality {
        match &self.measure {
            Some(measure) => measure.quality(peer).await,
            None => PeerQuality::Unknown,
        }
    }

    /// Return this node's local measurement counters for `peer`, if observed.
    pub(crate) async fn peer_measurement(&self, peer: Did) -> Option<PeerMeasurement> {
        match &self.measure {
            Some(measure) => match measure.peer_measurement(peer).await {
                Ok(measurement) => measurement,
                Err(error) => {
                    tracing::error!(peer = %peer, %error, "failed to project peer measurement");
                    None
                }
            },
            None => None,
        }
    }

    /// Return every retained local peer measurement.
    pub(crate) async fn peer_measurements(&self) -> Vec<PeerMeasurement> {
        match &self.measure {
            Some(measure) => match measure.peer_measurements().await {
                Ok(measurements) => measurements,
                Err(error) => {
                    tracing::error!(%error, "failed to project peer measurements");
                    Vec::new()
                }
            },
            None => Vec::new(),
        }
    }

    /// Return one bounded page of retained local peer measurements.
    pub(crate) async fn peer_measurements_page(
        &self,
        after: Option<Did>,
        limit: NonZeroUsize,
    ) -> PeerMeasurementPage {
        match &self.measure {
            Some(measure) => match measure.peer_measurements_page(after, limit).await {
                Ok(page) => page,
                Err(error) => {
                    tracing::error!(%error, "failed to project peer measurement page");
                    PeerMeasurementPage::default()
                }
            },
            None => PeerMeasurementPage::default(),
        }
    }

    /// Order DHT-produced connection candidates by local quality evidence.
    ///
    /// Invariant: this is a stable permutation of the DHT-produced candidate
    /// sequence. It changes attempt order only; it never changes Chord ownership,
    /// successor responsibility, or storage placement.
    pub(crate) async fn order_dht_candidates_by_quality(
        &self,
        candidates: impl IntoIterator<Item = Did>,
    ) -> Vec<Did> {
        let mut measured = Vec::new();
        for did in candidates {
            measured.push((did, self.peer_quality(did).await));
        }
        order_peers_by_quality(measured)
    }
}
