//! Constant-space, coalesced outbound measurement delivery.
//!
//! Producers aggregate logical outcome counts and useful-byte totals behind a
//! short synchronous lock. The one-slot channel is only a wake signal; a full
//! wake channel cannot lose state. Closing the producer drains the complete
//! aggregate before the worker exits.

use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::Mutex;

use futures::channel::mpsc;
use futures::StreamExt;

use crate::dht::Did;
use crate::measure::Authentication;
use crate::measure::MeasureImpl;
use crate::measure::MeasurementBatch;
use crate::measure::MeasurementEvent;

/// Capacity of the coalesced measurement wake channel.
const MEASUREMENT_WAKE_CAPACITY: usize = 1;

/// Logical outbound send outcome reported to the measurement aggregator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutboundMeasurement {
    /// A logical payload was sent, carrying `useful_bytes` of application/protocol payload.
    Sent {
        /// Useful payload bytes attributed to the sent transfer.
        useful_bytes: u64,
    },
    /// A logical payload failed to send.
    FailedToSend,
}

/// Discriminant used to alternate draining sent and failed measurement aggregates.
#[derive(Clone, Copy)]
enum OutboundMeasurementKind {
    /// Successful send aggregate.
    Sent,
    /// Failed send aggregate.
    FailedToSend,
}

impl OutboundMeasurementKind {
    /// Return the complementary measurement kind for round-robin draining.
    const fn other(self) -> Self {
        match self {
            Self::Sent => Self::FailedToSend,
            Self::FailedToSend => Self::Sent,
        }
    }
}

/// Coalesced counts and byte totals awaiting delivery to the measurement sink.
#[derive(Default)]
struct PendingState {
    /// Number of coalesced successful sends.
    sent_count: u64,
    /// Sum of useful bytes across coalesced successful sends.
    sent_bytes: u64,
    /// Number of coalesced send failures.
    failed_to_send: u64,
}

impl PendingState {
    /// Fold `measurement` into the pending totals, returning `false` on arithmetic overflow.
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

    /// Take and reset the pending batch for `kind`, if non-empty.
    fn take(&mut self, kind: OutboundMeasurementKind) -> Option<MeasurementBatch> {
        match kind {
            OutboundMeasurementKind::Sent if self.sent_count > 0 => {
                let occurrences = NonZeroU64::new(std::mem::take(&mut self.sent_count))?;
                let useful_bytes = std::mem::take(&mut self.sent_bytes);
                Some(MeasurementBatch::new(
                    MeasurementEvent::Sent { useful_bytes },
                    occurrences,
                ))
            }
            OutboundMeasurementKind::FailedToSend if self.failed_to_send > 0 => {
                let occurrences = NonZeroU64::new(std::mem::take(&mut self.failed_to_send))?;
                Some(MeasurementBatch::new(
                    MeasurementEvent::FailedToSend,
                    occurrences,
                ))
            }
            _ => None,
        }
    }

    /// Take the next non-empty batch, checking `first` before its counterpart.
    fn take_next(
        &mut self,
        first: OutboundMeasurementKind,
    ) -> Option<(OutboundMeasurementKind, MeasurementBatch)> {
        [first, first.other()]
            .into_iter()
            .find_map(|kind| self.take(kind).map(|batch| (kind, batch)))
    }
}

/// Thread-safe wrapper around [`PendingState`] shared by recorder and receiver.
struct PendingMeasurements {
    /// Synchronized pending measurement counters.
    state: Mutex<PendingState>,
}

impl PendingMeasurements {
    /// Create an empty pending measurement aggregate.
    fn new() -> Self {
        Self {
            state: Mutex::new(PendingState::default()),
        }
    }

    /// Record `measurement` into the shared aggregate.
    fn increment(&self, measurement: OutboundMeasurement) -> bool {
        lock_or_recover(&self.state).increment(measurement)
    }

    /// Take the next available measurement batch, starting with `first`.
    fn take_next(
        &self,
        first: OutboundMeasurementKind,
    ) -> Option<(OutboundMeasurementKind, MeasurementBatch)> {
        lock_or_recover(&self.state).take_next(first)
    }
}

/// Lock `mutex`, recovering the inner guard if a prior holder panicked.
fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Producer handle that coalesces outbound measurements and wakes the receiver task.
pub(super) struct MeasurementRecorder {
    /// Single-slot wake channel signaling that pending measurements are ready.
    sender: mpsc::Sender<()>,
    /// Shared pending aggregate updated synchronously under a short lock.
    pending: Arc<PendingMeasurements>,
    /// Whether a measurement sink is configured and recording is active.
    enabled: bool,
}

impl MeasurementRecorder {
    /// Create a paired recorder and receiver for peer `did` backed by `measure`.
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

    /// Aggregate `measurement` and signal the receiver task if enabled.
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

/// Consumer task that drains coalesced outbound measurements into the peer's `MeasureImpl`.
pub(super) struct MeasurementReceiver {
    /// Wake signal receiver driven by [`MeasurementRecorder`].
    receiver: mpsc::Receiver<()>,
    /// Shared pending aggregate drained on each wakeup and before exit.
    pending: Arc<PendingMeasurements>,
    /// Optional measurement sink receiving aggregated batches.
    measure: Option<MeasureImpl>,
    /// Peer DID attributed to every recorded batch.
    did: Did,
    /// Measurement kind checked first on the next drain cycle for fairness.
    next: OutboundMeasurementKind,
}

impl MeasurementReceiver {
    /// Drain coalesced measurement batches until the recorder is dropped.
    pub(super) async fn run(mut self) {
        loop {
            while let Some((kind, batch)) = self.pending.take_next(self.next) {
                self.next = kind.other();
                self.record(batch).await;
            }
            if self.receiver.next().await.is_none() {
                return;
            }
        }
    }

    /// Apply a single aggregated `batch` to the configured measurement sink.
    async fn record(&self, batch: MeasurementBatch) {
        let Some(measure) = &self.measure else {
            return;
        };
        if let Err(error) = measure
            .record_batch(self.did, Authentication::Authenticated, batch)
            .await
        {
            tracing::error!(
                peer = %self.did,
                batch = ?batch,
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
    use crate::measure::MeasureError;
    use crate::measure::PeerQuality;

    #[derive(Default)]
    struct RecordingMeasure {
        batches: Mutex<Vec<MeasurementBatch>>,
    }

    #[async_trait]
    impl Measure for RecordingMeasure {
        async fn record(
            &self,
            _did: Did,
            authentication: Authentication,
            event: MeasurementEvent,
        ) -> Result<ApplyOutcome, MeasureError> {
            if !authentication.permits(event) {
                return Ok(ApplyOutcome::IgnoredUnattributable);
            }
            lock_or_recover(&self.batches).push(MeasurementBatch::single(event));
            Ok(ApplyOutcome::Applied)
        }

        async fn record_batch(
            &self,
            _did: Did,
            authentication: Authentication,
            batch: MeasurementBatch,
        ) -> Result<ApplyOutcome, MeasureError> {
            if !authentication.permits(batch.event()) {
                return Ok(ApplyOutcome::IgnoredUnattributable);
            }
            lock_or_recover(&self.batches).push(batch);
            Ok(ApplyOutcome::Applied)
        }
    }

    #[async_trait]
    impl BehaviourJudgement for RecordingMeasure {
        async fn quality(&self, _did: Did) -> PeerQuality {
            PeerQuality::Unknown
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

        let batches = lock_or_recover(&measure.batches);
        assert_eq!(batches.len(), 2);
        assert_eq!(
            batches
                .iter()
                .filter(|batch| matches!(batch.event(), MeasurementEvent::Sent { .. }))
                .map(|batch| batch.occurrences().get())
                .sum::<u64>(),
            2
        );
        assert_eq!(
            batches
                .iter()
                .filter_map(|batch| match batch.event() {
                    MeasurementEvent::Sent { useful_bytes } => Some(useful_bytes),
                    _ => None,
                })
                .sum::<u64>(),
            8
        );
        assert!(batches.iter().any(|batch| {
            matches!(batch.event(), MeasurementEvent::FailedToSend)
                && batch.occurrences().get() == 1
        }));
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
