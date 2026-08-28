//! Constant-space, coalesced outbound measurement delivery.
//!
//! Producers aggregate logical outcome counts and useful-byte totals behind a
//! short synchronous lock. The one-slot channel is only a wake signal; a full
//! wake channel cannot lose state. Closing the producer drains the complete
//! aggregate before the worker exits.

use std::sync::Arc;
use std::sync::Mutex;

use futures::channel::mpsc;
use futures::StreamExt;

use crate::dht::Did;
use crate::measure::MeasureImpl;
use crate::measure::MeasurementEvent;

const MEASUREMENT_WAKE_CAPACITY: usize = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutboundMeasurement {
    Sent { useful_bytes: u64 },
    FailedToSend,
}

impl OutboundMeasurement {
    const fn kind(self) -> OutboundMeasurementKind {
        match self {
            Self::Sent { .. } => OutboundMeasurementKind::Sent,
            Self::FailedToSend => OutboundMeasurementKind::FailedToSend,
        }
    }

    const fn event(self) -> MeasurementEvent {
        match self {
            Self::Sent { useful_bytes } => MeasurementEvent::Sent { useful_bytes },
            Self::FailedToSend => MeasurementEvent::FailedToSend,
        }
    }
}

#[derive(Clone, Copy)]
enum OutboundMeasurementKind {
    Sent,
    FailedToSend,
}

impl OutboundMeasurementKind {
    const fn other(self) -> Self {
        match self {
            Self::Sent => Self::FailedToSend,
            Self::FailedToSend => Self::Sent,
        }
    }
}

#[derive(Default)]
struct PendingState {
    sent_count: u64,
    sent_bytes: u64,
    failed_to_send: u64,
}

impl PendingState {
    fn increment(&mut self, measurement: OutboundMeasurement) -> bool {
        match measurement {
            OutboundMeasurement::Sent { useful_bytes } => {
                let Some(sent_count) = self.sent_count.checked_add(1) else {
                    return false;
                };
                let Some(sent_bytes) = self.sent_bytes.checked_add(useful_bytes) else {
                    return false;
                };
                self.sent_count = sent_count;
                self.sent_bytes = sent_bytes;
            }
            OutboundMeasurement::FailedToSend => {
                let Some(failed_to_send) = self.failed_to_send.checked_add(1) else {
                    return false;
                };
                self.failed_to_send = failed_to_send;
            }
        }
        true
    }

    fn take(&mut self, kind: OutboundMeasurementKind) -> Option<OutboundMeasurement> {
        match kind {
            OutboundMeasurementKind::Sent if self.sent_count > 0 => {
                self.sent_count -= 1;
                // Constant-space coalescing preserves logical count and total
                // bytes, not the association between each send and its size.
                // The accumulated byte total is attached to the final replayed
                // sent event; the ledger observes the exact count and sum.
                let useful_bytes = if self.sent_count == 0 {
                    std::mem::take(&mut self.sent_bytes)
                } else {
                    0
                };
                Some(OutboundMeasurement::Sent { useful_bytes })
            }
            OutboundMeasurementKind::FailedToSend if self.failed_to_send > 0 => {
                self.failed_to_send -= 1;
                Some(OutboundMeasurement::FailedToSend)
            }
            _ => None,
        }
    }

    fn take_next(&mut self, first: OutboundMeasurementKind) -> Option<OutboundMeasurement> {
        [first, first.other()]
            .into_iter()
            .find_map(|kind| self.take(kind))
    }
}

struct PendingMeasurements {
    state: Mutex<PendingState>,
}

impl PendingMeasurements {
    fn new() -> Self {
        Self {
            state: Mutex::new(PendingState::default()),
        }
    }

    fn increment(&self, measurement: OutboundMeasurement) -> bool {
        lock_or_recover(&self.state).increment(measurement)
    }

    fn take_next(&self, first: OutboundMeasurementKind) -> Option<OutboundMeasurement> {
        lock_or_recover(&self.state).take_next(first)
    }
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) struct MeasurementRecorder {
    sender: mpsc::Sender<()>,
    pending: Arc<PendingMeasurements>,
    enabled: bool,
}

impl MeasurementRecorder {
    pub(super) fn channel(measure: Option<MeasureImpl>, did: Did) -> (Self, MeasurementReceiver) {
        let (sender, receiver) = mpsc::channel(MEASUREMENT_WAKE_CAPACITY);
        let pending = Arc::new(PendingMeasurements::new());
        (
            Self {
                sender,
                pending: pending.clone(),
                enabled: measure.is_some(),
            },
            MeasurementReceiver {
                receiver,
                pending,
                measure,
                did,
                next: OutboundMeasurementKind::Sent,
            },
        )
    }

    pub(super) fn record(&mut self, measurement: OutboundMeasurement) {
        if !self.enabled {
            return;
        }
        if !self.pending.increment(measurement) {
            tracing::error!("outbound measurement aggregate overflowed");
            return;
        }
        let _ = self.sender.try_send(());
    }
}

pub(super) struct MeasurementReceiver {
    receiver: mpsc::Receiver<()>,
    pending: Arc<PendingMeasurements>,
    measure: Option<MeasureImpl>,
    did: Did,
    next: OutboundMeasurementKind,
}

impl MeasurementReceiver {
    pub(super) async fn run(mut self) {
        loop {
            while let Some(measurement) = self.pending.take_next(self.next) {
                self.next = measurement.kind().other();
                self.record(measurement).await;
            }
            if self.receiver.next().await.is_none() {
                return;
            }
        }
    }

    async fn record(&self, measurement: OutboundMeasurement) {
        let Some(measure) = &self.measure else {
            return;
        };
        if let Err(error) = measure.record(self.did, measurement.event()).await {
            tracing::error!(
                peer = %self.did,
                event = ?measurement.event(),
                %error,
                "failed to apply outbound measurement"
            );
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
#[allow(clippy::panic)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::measure::ApplyOutcome;
    use crate::measure::BehaviourJudgement;
    use crate::measure::Measure;
    use crate::measure::MeasureCounter;
    use crate::measure::MeasureError;
    use crate::measure::PeerQuality;

    #[derive(Default)]
    struct RecordingMeasure {
        events: Mutex<Vec<MeasurementEvent>>,
    }

    #[async_trait]
    impl Measure for RecordingMeasure {
        async fn incr(&self, _did: Did, _counter: MeasureCounter) {}

        async fn get_count(&self, _did: Did, _counter: MeasureCounter) -> u64 {
            0
        }

        async fn record(
            &self,
            _did: Did,
            event: MeasurementEvent,
        ) -> Result<ApplyOutcome, MeasureError> {
            lock_or_recover(&self.events).push(event);
            Ok(ApplyOutcome::Applied)
        }
    }

    #[async_trait]
    impl BehaviourJudgement for RecordingMeasure {
        async fn quality(&self, _did: Did) -> PeerQuality {
            PeerQuality::Unknown
        }

        async fn good(&self, _did: Did) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn coalescing_preserves_logical_count_and_total_useful_bytes() {
        let measure = Arc::new(RecordingMeasure::default());
        let did = Did::from(7_u32);
        let implementation: MeasureImpl = measure.clone();
        let (mut recorder, receiver) = MeasurementRecorder::channel(Some(implementation), did);
        recorder.record(OutboundMeasurement::Sent { useful_bytes: 3 });
        recorder.record(OutboundMeasurement::Sent { useful_bytes: 5 });
        recorder.record(OutboundMeasurement::FailedToSend);
        drop(recorder);
        receiver.run().await;

        let events = lock_or_recover(&measure.events);
        assert_eq!(events.len(), 3);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, MeasurementEvent::Sent { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    MeasurementEvent::Sent { useful_bytes } => Some(*useful_bytes),
                    _ => None,
                })
                .sum::<u64>(),
            8
        );
        assert!(events.contains(&MeasurementEvent::FailedToSend));
    }

    #[tokio::test]
    async fn disabled_recorder_retains_no_events() {
        let did = Did::from(7_u32);
        let (mut recorder, receiver) = MeasurementRecorder::channel(None, did);
        recorder.record(OutboundMeasurement::Sent { useful_bytes: 3 });
        drop(recorder);
        receiver.run().await;
    }
}
