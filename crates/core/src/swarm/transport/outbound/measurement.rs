use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use futures::StreamExt;

use crate::dht::Did;
use crate::measure::MeasureCounter;
use crate::measure::MeasureImpl;
use crate::utils::sleep;

const MEASUREMENT_WAKE_CAPACITY: usize = 1;
const MAX_PENDING_MEASUREMENTS: usize = 2048;
const MEASUREMENT_SHUTDOWN_DRAIN_LIMIT: usize = 16;
#[cfg(test)]
const MEASUREMENT_RECORD_TIMEOUT: Duration = Duration::from_millis(50);
#[cfg(not(test))]
const MEASUREMENT_RECORD_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
pub(super) enum OutboundMeasurement {
    Sent,
    FailedToSend,
}

impl OutboundMeasurement {
    const fn counter(self) -> MeasureCounter {
        match self {
            Self::Sent => MeasureCounter::Sent,
            Self::FailedToSend => MeasureCounter::FailedToSend,
        }
    }

    const fn other(self) -> Self {
        match self {
            Self::Sent => Self::FailedToSend,
            Self::FailedToSend => Self::Sent,
        }
    }
}

struct PendingMeasurements {
    sent: AtomicUsize,
    failed_to_send: AtomicUsize,
    total: AtomicUsize,
}

impl PendingMeasurements {
    fn new() -> Self {
        Self {
            sent: AtomicUsize::new(0),
            failed_to_send: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
        }
    }

    fn increment(&self, measurement: OutboundMeasurement) -> bool {
        if self
            .total
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                (value < MAX_PENDING_MEASUREMENTS).then_some(value + 1)
            })
            .is_err()
        {
            return false;
        }
        let pending = self.counter(measurement);
        let _ = pending.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            Some(value.saturating_add(1))
        });
        true
    }

    fn next(&self, first: OutboundMeasurement) -> Option<OutboundMeasurement> {
        [first, first.other()]
            .into_iter()
            .find(|measurement| self.counter(*measurement).load(Ordering::Acquire) > 0)
    }

    fn complete_one(&self, measurement: OutboundMeasurement) {
        if self
            .counter(measurement)
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            })
            .is_err()
        {
            tracing::error!("completed an outbound measurement with no pending count");
            return;
        }
        let _ = self
            .total
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            });
    }

    fn clear(&self) {
        self.sent.store(0, Ordering::Release);
        self.failed_to_send.store(0, Ordering::Release);
        self.total.store(0, Ordering::Release);
    }

    const fn counter(&self, measurement: OutboundMeasurement) -> &AtomicUsize {
        match measurement {
            OutboundMeasurement::Sent => &self.sent,
            OutboundMeasurement::FailedToSend => &self.failed_to_send,
        }
    }
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
                next: OutboundMeasurement::Sent,
            },
        )
    }

    pub(super) fn record(&mut self, measurement: OutboundMeasurement) {
        if !self.enabled {
            return;
        }
        if !self.pending.increment(measurement) {
            tracing::warn!("outbound measurement backlog is full; dropping measurement");
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
    next: OutboundMeasurement,
}

impl MeasurementReceiver {
    pub(super) async fn run(mut self) {
        let mut shutdown_remaining: Option<usize> = None;
        loop {
            if let Some(measurement) = self.pending.next(self.next) {
                let outcome = self.record(measurement).await;
                self.pending.complete_one(measurement);
                self.next = measurement.other();
                if let Some(remaining) = shutdown_remaining.as_mut() {
                    *remaining = remaining.saturating_sub(1);
                    if matches!(outcome, MeasurementRecordOutcome::TimedOut) || *remaining == 0 {
                        self.pending.clear();
                        return;
                    }
                } else if self.receiver_is_closed() {
                    if matches!(outcome, MeasurementRecordOutcome::TimedOut) {
                        self.pending.clear();
                        return;
                    }
                    shutdown_remaining = Some(MEASUREMENT_SHUTDOWN_DRAIN_LIMIT);
                }
                continue;
            }
            if self.receiver.next().await.is_none() {
                return;
            }
        }
    }

    fn receiver_is_closed(&mut self) -> bool {
        loop {
            match self.receiver.try_recv() {
                Ok(()) => {}
                Err(error) if error.is_closed() => return true,
                Err(_) => return false,
            }
        }
    }

    async fn record(&self, measurement: OutboundMeasurement) -> MeasurementRecordOutcome {
        let Some(measure) = &self.measure else {
            return MeasurementRecordOutcome::Recorded;
        };
        let record = measure.incr(self.did, measurement.counter()).fuse();
        let timeout = sleep(MEASUREMENT_RECORD_TIMEOUT).fuse();
        pin_mut!(record, timeout);
        select! {
            _ = record => MeasurementRecordOutcome::Recorded,
            _ = timeout => {
                tracing::warn!(
                    peer = %self.did,
                    counter = ?measurement.counter(),
                    timeout_ms = MEASUREMENT_RECORD_TIMEOUT.as_millis(),
                    "outbound measurement recording timed out"
                );
                MeasurementRecordOutcome::TimedOut
            }
        }
    }
}

#[derive(Clone, Copy)]
enum MeasurementRecordOutcome {
    Recorded,
    TimedOut,
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;

    use super::*;
    use crate::measure::BehaviourJudgement;
    use crate::measure::Measure;
    use crate::measure::PeerQuality;
    use crate::utils::yield_executor_once;

    struct CountingMeasure {
        calls: Arc<AtomicUsize>,
        started: Arc<AtomicBool>,
        pending: bool,
        released: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Measure for CountingMeasure {
        async fn incr(&self, _did: Did, _counter: MeasureCounter) {
            self.started.store(true, Ordering::Release);
            if self.pending {
                while !self.released.load(Ordering::Acquire) {
                    yield_executor_once().await;
                }
            }
            self.calls.fetch_add(1, Ordering::AcqRel);
        }

        async fn get_count(&self, _did: Did, _counter: MeasureCounter) -> u64 {
            0
        }
    }

    #[async_trait]
    impl BehaviourJudgement for CountingMeasure {
        async fn quality(&self, _did: Did) -> PeerQuality {
            PeerQuality::Unknown
        }

        async fn good(&self, _did: Did) -> bool {
            true
        }
    }

    struct OrderedMeasure {
        calls: Arc<Mutex<Vec<MeasureCounter>>>,
    }

    #[async_trait]
    impl Measure for OrderedMeasure {
        async fn incr(&self, _did: Did, counter: MeasureCounter) {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(counter);
        }

        async fn get_count(&self, _did: Did, _counter: MeasureCounter) -> u64 {
            0
        }
    }

    #[async_trait]
    impl BehaviourJudgement for OrderedMeasure {
        async fn quality(&self, _did: Did) -> PeerQuality {
            PeerQuality::Unknown
        }

        async fn good(&self, _did: Did) -> bool {
            true
        }
    }

    fn test_measure(
        pending: bool,
    ) -> (
        MeasureImpl,
        Arc<AtomicUsize>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(!pending));
        let measure: MeasureImpl = Arc::new(CountingMeasure {
            calls: calls.clone(),
            started: started.clone(),
            pending,
            released: released.clone(),
        });
        (measure, calls, started, released)
    }

    #[tokio::test]
    async fn closed_measurement_receiver_drains_buffered_events() {
        let (measure, calls, _, _) = test_measure(false);
        let (mut recorder, receiver) =
            MeasurementRecorder::channel(Some(measure), Did::from(1_u32));
        recorder.record(OutboundMeasurement::Sent);
        recorder.record(OutboundMeasurement::FailedToSend);

        drop(recorder);
        receiver.run().await;

        assert_eq!(calls.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn full_wake_channel_coalesces_without_losing_measurements() {
        let (measure, calls, _, _) = test_measure(false);
        let (mut recorder, receiver) =
            MeasurementRecorder::channel(Some(measure), Did::from(3_u32));
        for index in 0..1_024 {
            let measurement = if index % 2 == 0 {
                OutboundMeasurement::Sent
            } else {
                OutboundMeasurement::FailedToSend
            };
            recorder.record(measurement);
        }
        let run = tokio::spawn(receiver.run());
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::Acquire) < 1_024 {
                yield_executor_once().await;
            }
        })
        .await
        .expect("active receiver must drain every coalesced measurement");
        drop(recorder);
        run.await.expect("measurement receiver must not panic");

        assert_eq!(calls.load(Ordering::Acquire), 1_024);
    }

    #[tokio::test]
    async fn failed_measurement_is_not_starved_by_sent_backlog() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let measure: MeasureImpl = Arc::new(OrderedMeasure {
            calls: calls.clone(),
        });
        let (mut recorder, receiver) =
            MeasurementRecorder::channel(Some(measure), Did::from(4_u32));
        for _ in 0..1_024 {
            recorder.record(OutboundMeasurement::Sent);
        }
        recorder.record(OutboundMeasurement::FailedToSend);
        let run = tokio::spawn(receiver.run());
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let len = calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .len();
                if len == 1_025 {
                    break;
                }
                yield_executor_once().await;
            }
        })
        .await
        .expect("active receiver must drain the measurement backlog");
        drop(recorder);
        run.await.expect("measurement receiver must not panic");

        let calls = calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(calls.get(1), Some(&MeasureCounter::FailedToSend));
        assert_eq!(calls.len(), 1_025);
    }

    #[tokio::test]
    async fn pending_count_is_retained_until_recording_completes() {
        let (measure, calls, started, released) = test_measure(true);
        let (mut recorder, receiver) =
            MeasurementRecorder::channel(Some(measure), Did::from(2_u32));
        recorder.record(OutboundMeasurement::Sent);
        let pending = receiver.pending.clone();
        let run = tokio::spawn(receiver.run());
        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(Ordering::Acquire) {
                yield_executor_once().await;
            }
        })
        .await
        .expect("measurement must start");

        assert_eq!(pending.sent.load(Ordering::Acquire), 1);
        released.store(true, Ordering::Release);
        drop(recorder);
        tokio::time::timeout(Duration::from_secs(1), run)
            .await
            .expect("measurement receiver must drain after release")
            .expect("measurement receiver task must not panic");

        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(pending.sent.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn blocked_measurement_is_cancelled_and_receiver_exits() {
        let (measure, calls, started, _) = test_measure(true);
        let (mut recorder, receiver) =
            MeasurementRecorder::channel(Some(measure), Did::from(5_u32));
        for _ in 0..MAX_PENDING_MEASUREMENTS {
            recorder.record(OutboundMeasurement::Sent);
        }
        let pending = receiver.pending.clone();
        drop(recorder);

        tokio::time::timeout(Duration::from_secs(1), receiver.run())
            .await
            .expect("blocked measurement must be cancelled by its deadline");

        assert!(started.load(Ordering::Acquire));
        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert_eq!(pending.sent.load(Ordering::Acquire), 0);
        assert_eq!(pending.total.load(Ordering::Acquire), 0);
    }
}
