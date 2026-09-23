//! Per-peer outbound transfer scheduling.
//!
//! Each class owns one FIFO lane and admits no second transfer before its active
//! transfer finishes. Runnable heads use bounded DHT-control priority and
//! round-robin service for storage, E2E, and application traffic.
//! Cross-class order is not preserved; ordered sequences must stay in one class.
//! Each iteration handles the available command backlog and completed deliveries,
//! then admits at most one frame before choosing again.
//! This boundary can consume one send-accept budget and, after an irrevocable
//! timeout, one bounded close interval before another lane runs.
//!
//! A peer admits at most `OUTBOUND_TRANSFER_QUEUE_CAPACITY` transfers across
//! the command channel, lane queues, and delivery waits. Shutdown closes the
//! command channel synchronously; the worker then cancels every admitted
//! transfer and drops outstanding delivery futures.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::Weak;

use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use rings_transport::delivery::DeliveryFuture;

use super::delivery::await_delivery_or_cancel;
use super::delivery::frame_chunk;
use super::delivery::send_data_with_timeout;
use super::delivery::ChunkSendCancelReason;
use super::delivery::ChunkSendPermit;
use super::delivery::ChunkSendProgress;
use super::delivery::SendCompletionOutcome;
use super::delivery::TransferStop;
use super::AdmittedConnection;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::lifecycle::StopSource;
use crate::measure::MeasureImpl;
use crate::utils::get_epoch_ms;

mod admission;
mod capacity;
mod link_state;
mod mailbox;
mod measurement;
mod model;
mod queue;
mod session_encoding;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
mod simulation_pressure;
mod spawn;
#[cfg(test)]
mod test_trace;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use link_state::LINK_CONTROL_IN_FLIGHT_CAPACITY;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use test_trace::dispatched_link_control_for_test;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use test_trace::outbound_submit_count_for_test;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(super) use test_trace::record_dispatched_link_control;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use test_trace::referenced_slots_for_test;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use test_trace::reset_outbound_submit_count_for_test;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use test_trace::LinkDirection;
mod transfer;

pub(super) use admission::DetachedAdmission;
pub(super) use admission::DetachedAdmissionCancel;
pub(super) use admission::DetachedAdmissionClaim;
use capacity::GlobalTransferCapacity;
use capacity::TransferCapacity;
pub(super) use capacity::TransferCapacityPermit;
#[cfg(test)]
pub(crate) use capacity::OUTBOUND_CONTROL_RESERVED_TRANSFERS;
#[cfg(test)]
pub(crate) use capacity::OUTBOUND_DATA_TRANSFER_CAPACITY;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use capacity::OUTBOUND_GLOBAL_BYTE_CAPACITY;
#[cfg(test)]
pub(crate) use capacity::OUTBOUND_TRANSFER_QUEUE_CAPACITY;
pub(super) use link_state::LinkControlPermit;
use link_state::PeerLinkState;
use mailbox::MailboxReceiver;
use mailbox::MailboxSender;
use measurement::MeasurementReceiver;
use measurement::MeasurementRecorder;
use measurement::OutboundMeasurement;
pub(super) use model::OutboundCompletion;
pub(super) use model::OutboundMessageKind;
pub(super) use model::TransferClass;
use queue::RunnableTransfer;
use queue::TransferQueues;
#[cfg(test)]
pub(crate) use queue::OUTBOUND_CONTROL_BURST;
use session_encoding::SharedAnnouncedDelegations;
use spawn::spawn_worker;
pub(super) use transfer::ChunkFrames;
pub(super) use transfer::ChunkedFrameSource;
use transfer::FinalTransferResult;
pub(super) use transfer::OutboundTransfer;
pub(super) use transfer::OutboundTransferRoute;
use transfer::ShutdownBatch;

struct ScheduledTransfer<T = OutboundTransfer, P = TransferCapacityPermit> {
    transfer: T,
    capacity_permit: Option<P>,
}

impl<T, P> ScheduledTransfer<T, P> {
    fn new(transfer: T, capacity_permit: P) -> Self {
        Self {
            transfer,
            capacity_permit: Some(capacity_permit),
        }
    }
}

impl<T, P> Drop for ScheduledTransfer<T, P> {
    fn drop(&mut self) {
        // Completion senders may live inside `transfer`. Release admission
        // before Rust drops that field and wakes a receiver.
        self.capacity_permit.take();
    }
}

/// One mailbox command. The worker handles a batch of submissions in FIFO order,
/// through [`OutboundWorker::handle_commands`]. Only shutdown bypasses that
/// dispatcher, after closing ingress and cancelling all admitted transfers.
enum OutboundCommand {
    /// A transfer admitted by the submitter; rejected on arrival if its stop
    /// token is already set.
    Submit(Box<ScheduledTransfer>),
    /// Some transfer's stop token was set after it was submitted: cancel
    /// every queued transfer whose token is set.
    CancelStopped,
}

struct QueuedTransfer {
    id: u64,
    scheduled: ScheduledTransfer,
}

struct DeliveryEvent {
    id: u64,
    class: TransferClass,
    result: ChunkSendProgress<Result<()>>,
}

#[derive(Clone, Copy)]
enum TerminationFairness {
    AlreadyAdvanced,
    AdvanceFailedAttempt,
}

enum ActiveFrameStep {
    Stopped,
    Complete,
    Failed(Error),
    Send {
        class: TransferClass,
        before_first_frame: bool,
        bytes: bytes::Bytes,
        context: &'static str,
    },
}

fn resolve_cancelled_transfer(
    before_first_frame: bool,
    reason: ChunkSendCancelReason,
) -> Result<SendCompletionOutcome> {
    if before_first_frame {
        reason
            .resolve_initial()
            .map(|()| SendCompletionOutcome::Cancelled)
    } else {
        Ok(SendCompletionOutcome::Cancelled)
    }
}

#[derive(Clone)]
pub(super) struct OutboundPeerHandle {
    state: Arc<OutboundPeerState>,
}

struct OutboundPeerState {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    peer: Did,
    sender: MailboxSender<OutboundCommand>,
    /// The link's tables, kept across worker replacements under one generation.
    link: PeerLinkState,
    // Strong lifetime anchor; the peer registry intentionally stores only a Weak reference.
    _capacity_anchor: TransferCapacityAnchor,
    stop: StopSource,
}

struct TransferCapacityAnchor {
    _capacity: Arc<TransferCapacity>,
}

impl TransferCapacityAnchor {
    fn new(capacity: Arc<TransferCapacity>) -> Self {
        Self {
            _capacity: capacity,
        }
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    fn try_acquire(
        &self,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<TransferCapacityPermit> {
        self._capacity.try_acquire(peer, class, bytes)
    }
}

impl OutboundPeerHandle {
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(super) fn reserve(
        &self,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<TransferCapacityPermit> {
        if self.state.stop.is_stop_requested() {
            return Err(Error::ChannelSendMessageFailed);
        }
        self.state._capacity_anchor.try_acquire(peer, class, bytes)
    }

    pub(super) fn submit(
        &self,
        transfer: OutboundTransfer,
        capacity_permit: TransferCapacityPermit,
    ) -> Result<()> {
        if self.state.stop.is_stop_requested() {
            return Err(Error::ChannelSendMessageFailed);
        }
        let mut transfer = transfer;
        transfer.bind_scheduler_stop(self.state.stop.token());
        let scheduled = ScheduledTransfer::new(transfer, capacity_permit);
        if self.state.stop.is_stop_requested() {
            return Err(Error::ChannelSendMessageFailed);
        }
        let command = OutboundCommand::Submit(Box::new(scheduled));
        let (result, submitted) = match self.state.sender.send_if(command, |command| {
            matches!(command, OutboundCommand::Submit(scheduled) if !scheduled.transfer.is_stopped())
        }) {
            Ok(()) => (Ok(()), true),
            Err(OutboundCommand::Submit(scheduled)) if scheduled.transfer.is_stopped() => {
                if let Some(final_result) = OutboundWorker::cancel_scheduled_transfer(*scheduled) {
                    final_result.publish();
                }
                (Ok(()), false)
            }
            Err(_) => (Err(Error::ChannelSendMessageFailed), false),
        };
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        if submitted {
            test_trace::record_outbound_submit();
            test_trace::record_submission(self.state.peer);
        }
        #[cfg(not(all(test, feature = "dummy", not(target_family = "wasm"))))]
        let _ = submitted;
        result
    }

    pub(super) fn cancel_stopped(&self) {
        let _ = self
            .state
            .sender
            .send_coalesced(OutboundCommand::CancelStopped);
    }

    fn shutdown(&self) {
        self.state.shutdown();
    }
}

impl OutboundPeerState {
    fn shutdown(&self) {
        self.stop.request_stop();
        self.sender.close();
    }
}

impl Drop for OutboundPeerState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(super) struct OutboundSchedulers {
    registry: Mutex<OutboundRegistry>,
    global_capacity: Arc<GlobalTransferCapacity>,
    measure: Option<MeasureImpl>,
}

#[derive(Default)]
struct OutboundRegistry {
    peers: BTreeMap<Did, OutboundPeerHandle>,
    capacities: BTreeMap<Did, Weak<TransferCapacity>>,
}

impl OutboundRegistry {
    fn prune_capacities(&mut self) {
        self.capacities
            .retain(|_, capacity| capacity.strong_count() > 0);
    }

    fn capacity(
        &mut self,
        peer: Did,
        global: &Arc<GlobalTransferCapacity>,
    ) -> Arc<TransferCapacity> {
        self.prune_capacities();
        if let Some(capacity) = self.capacities.get(&peer).and_then(Weak::upgrade) {
            return capacity;
        }
        let capacity = Arc::new(TransferCapacity::new(global.clone()));
        self.capacities.insert(peer, Arc::downgrade(&capacity));
        capacity
    }
}

impl OutboundSchedulers {
    pub(super) fn new(measure: Option<MeasureImpl>) -> Self {
        Self {
            registry: Mutex::new(OutboundRegistry::default()),
            global_capacity: Arc::new(GlobalTransferCapacity::new()),
            measure,
        }
    }

    pub(super) fn handle(&self, peer: Did) -> Result<OutboundPeerHandle> {
        let mut registry = self.lock_registry();
        if let Some(handle) = registry
            .peers
            .get(&peer)
            .filter(|handle| !handle.state.stop.is_stop_requested())
        {
            return Ok(handle.clone());
        }
        // A worker replaced under an unchanged connection generation keeps the link's tables:
        // what the old worker announced stays answerable, and the sends still in flight stay
        // counted. A newer generation empties the announced table by itself on its first frame.
        let link = match registry.peers.remove(&peer) {
            Some(stopped) => {
                let link = stopped.state.link.clone();
                stopped.shutdown();
                link
            }
            None => PeerLinkState::new(),
        };
        let capacity = registry.capacity(peer, &self.global_capacity);
        let (sender, receiver) = mailbox::channel();
        let stop = StopSource::new();
        let state = Arc::new(OutboundPeerState {
            #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
            peer,
            sender,
            link: link.clone(),
            _capacity_anchor: TransferCapacityAnchor::new(capacity),
            stop: stop.clone(),
        });
        let handle = OutboundPeerHandle { state };
        let (measurements, measurement_receiver) =
            MeasurementRecorder::channel(self.measure.clone(), peer);
        spawn_worker(
            OutboundWorker::new(receiver, stop, measurements, peer, link.announced),
            measurement_receiver,
        )?;
        registry.peers.insert(peer, handle.clone());
        Ok(handle)
    }

    pub(super) async fn reserve(
        &self,
        peer: Did,
        class: TransferClass,
        bytes: usize,
    ) -> Result<TransferCapacityPermit> {
        let capacity = self.lock_registry().capacity(peer, &self.global_capacity);
        capacity.acquire(peer, class, bytes).await
    }

    pub(super) fn shutdown(&self, peer: Did) {
        let handle = self.lock_registry().peers.remove(&peer);
        if let Some(handle) = handle {
            handle.shutdown();
        }
        self.lock_registry().prune_capacities();
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(super) fn peer_count_for_test(&self) -> usize {
        self.lock_registry().peers.len()
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(super) fn capacity_key_count_for_test(&self) -> usize {
        self.lock_registry().capacities.len()
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    fn admitted_transfer_count_for_test(&self, peer: Did) -> Option<usize> {
        self.lock_registry()
            .capacities
            .get(&peer)
            .and_then(Weak::upgrade)
            .map(|capacity| capacity.admitted())
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    fn admitted_transfer_total_for_test(&self) -> usize {
        let mut registry = self.lock_registry();
        registry.prune_capacities();
        registry
            .capacities
            .values()
            .filter_map(Weak::upgrade)
            .map(|capacity| capacity.admitted())
            .sum()
    }

    fn lock_registry(&self) -> MutexGuard<'_, OutboundRegistry> {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for OutboundSchedulers {
    fn drop(&mut self) {
        let registry = match self.registry.get_mut() {
            Ok(registry) => registry,
            Err(poisoned) => poisoned.into_inner(),
        };
        for handle in registry.peers.values() {
            handle.shutdown();
        }
    }
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
type DeliveryWaitFuture = Pin<Box<dyn Future<Output = DeliveryEvent> + Send>>;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
type DeliveryWaitFuture = Pin<Box<dyn Future<Output = DeliveryEvent>>>;

struct OutboundWorker {
    #[cfg(test)]
    peer: Did,
    receiver: MailboxReceiver<OutboundCommand>,
    ready: TransferQueues<QueuedTransfer>,
    active: Option<RunnableTransfer<QueuedTransfer>>,
    announced: SharedAnnouncedDelegations,
    deliveries: FuturesUnordered<DeliveryWaitFuture>,
    measurements: MeasurementRecorder,
    stop: StopSource,
    next_id: u64,
    input_closed: bool,
    shutdown_complete: bool,
}

impl OutboundWorker {
    fn new(
        receiver: MailboxReceiver<OutboundCommand>,
        stop: StopSource,
        measurements: MeasurementRecorder,
        peer: Did,
        announced: SharedAnnouncedDelegations,
    ) -> Self {
        #[cfg(not(test))]
        let _ = peer;
        #[cfg(test)]
        let next_id = test_trace::worker_transfer_id_base();
        #[cfg(not(test))]
        let next_id = 0;
        Self {
            #[cfg(test)]
            peer,
            receiver,
            ready: TransferQueues::default(),
            active: None,
            announced,
            deliveries: FuturesUnordered::new(),
            measurements,
            stop,
            next_id,
            input_closed: false,
            shutdown_complete: false,
        }
    }

    async fn run(mut self) {
        loop {
            if self.stop.is_stop_requested() {
                self.shutdown();
                return;
            }
            #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
            while test_trace::worker_is_paused(self.peer) && !self.stop.is_stop_requested() {
                tokio::task::yield_now().await;
            }
            if self.stop.is_stop_requested() {
                self.shutdown();
                return;
            }
            self.drain_available();
            if self.stop.is_stop_requested() {
                self.shutdown();
                return;
            }
            if self.input_closed {
                self.shutdown();
                return;
            }
            if let Some(transfer) = self.ready.pop() {
                self.active = Some(transfer);
                self.admit_active_frame().await;
                continue;
            }
            self.wait_for_input().await;
        }
    }

    /// Collect at most 256 submissions and one coalesced cancellation scan.
    /// All control submissions in that batch are visible before selection;
    /// submissions racing the empty read may enter the next iteration. At most four
    /// lane heads can have completed deliveries, with no new waits added here.
    fn drain_available(&mut self) {
        let commands = self.receiver.drain_available();
        self.handle_commands(commands);
        self.input_closed = self.receiver.is_closed();

        while let Some(Some(event)) = self.deliveries.next().now_or_never() {
            self.handle_delivery(event);
        }
    }

    fn enqueue_transfer(&mut self, scheduled: ScheduledTransfer) {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let class = scheduled.transfer.class();
        self.ready.push(class, QueuedTransfer { id, scheduled });
    }

    /// Apply a finite batch (or one idle input), releasing cancelled ownership
    /// before publishing results. Scans never consume ingress; a stopped submit
    /// is rejected here even when its cancellation notification arrived first.
    fn handle_commands(&mut self, commands: impl IntoIterator<Item = OutboundCommand>) {
        // Defer publication until every collected command relinquishes ownership.
        let mut results = Vec::new();
        for command in commands {
            match command {
                OutboundCommand::Submit(transfer) if transfer.transfer.is_stopped() => {
                    results.extend(Self::cancel_scheduled_transfer(*transfer));
                }
                OutboundCommand::Submit(transfer) => self.enqueue_transfer(*transfer),
                OutboundCommand::CancelStopped => {
                    // Waiting heads stay owned by delivery; queued successors can stop.
                    results.extend(
                        self.ready
                            .remove_ready_where(|queued| queued.scheduled.transfer.is_stopped())
                            .into_iter()
                            .filter_map(|queued| Self::cancel_scheduled_transfer(queued.scheduled)),
                    );
                }
            }
        }
        if self.stop.is_stop_requested() {
            self.shutdown_with_results(results);
        } else {
            Self::publish_released_results(results);
        }
    }

    fn terminate_transfer(
        &mut self,
        transfer: RunnableTransfer<QueuedTransfer>,
        result: Result<SendCompletionOutcome>,
        fairness: TerminationFairness,
    ) {
        let queued = match fairness {
            TerminationFairness::AlreadyAdvanced => self.ready.finish_transfer(transfer),
            TerminationFairness::AdvanceFailedAttempt => self.ready.fail_attempt(transfer),
        };
        let final_result = Self::finalize_scheduled_transfer(queued.scheduled, result);
        if let Some(final_result) = final_result {
            final_result.publish();
        }
    }

    fn finalize_scheduled_transfer(
        mut scheduled: ScheduledTransfer,
        result: Result<SendCompletionOutcome>,
    ) -> Option<FinalTransferResult> {
        let final_result = scheduled.transfer.take_final(result);
        drop(scheduled);
        final_result
    }

    fn cancel_scheduled_transfer(scheduled: ScheduledTransfer) -> Option<FinalTransferResult> {
        Self::finalize_scheduled_transfer(scheduled, Ok(SendCompletionOutcome::Cancelled))
    }

    fn cancel_batch(batch: ShutdownBatch<ScheduledTransfer>) -> Vec<FinalTransferResult> {
        batch.finalize(Self::cancel_scheduled_transfer)
    }

    /// Publish a batch only after every source transfer has released its capacity permit.
    fn publish_released_results(final_results: Vec<FinalTransferResult>) {
        for final_result in final_results {
            final_result.publish();
        }
    }

    fn fail_transfer(
        &mut self,
        transfer: RunnableTransfer<QueuedTransfer>,
        error: Error,
        fairness: TerminationFairness,
    ) {
        let record_failure = error.records_peer_send_failure();
        self.terminate_transfer(transfer, Err(error), fairness);
        if record_failure {
            self.measurements.record(OutboundMeasurement::FailedToSend);
        }
    }

    fn handle_delivery(&mut self, event: DeliveryEvent) {
        let Some(transfer) = self.ready.take_waiting(event.class, event.id) else {
            debug_assert!(false, "delivery must identify the waiting lane head");
            return;
        };
        match event.result {
            ChunkSendProgress::Ready(Ok(())) => {
                self.ready.make_runnable(transfer);
            }
            ChunkSendProgress::Ready(Err(error)) => {
                self.fail_transfer(transfer, error, TerminationFairness::AlreadyAdvanced);
            }
            ChunkSendProgress::Cancelled(reason) => {
                let record_failure = reason.records_peer_failure();
                self.terminate_transfer(
                    transfer,
                    Ok(SendCompletionOutcome::Cancelled),
                    TerminationFairness::AlreadyAdvanced,
                );
                if record_failure {
                    self.measurements.record(OutboundMeasurement::FailedToSend);
                }
            }
        }
    }

    fn drain_ready_transfers(&mut self) -> Vec<ScheduledTransfer> {
        self.ready
            .drain_transfers()
            .into_iter()
            .map(|queued| queued.scheduled)
            .collect()
    }

    fn drain_active_transfers(&mut self) -> Vec<ScheduledTransfer> {
        self.active
            .take()
            .into_iter()
            .map(|transfer| transfer.into_parts().1.scheduled)
            .collect()
    }

    fn shutdown(&mut self) {
        self.shutdown_with_results(Vec::new());
    }

    fn shutdown_with_results(&mut self, mut final_results: Vec<FinalTransferResult>) {
        if self.shutdown_complete {
            return;
        }
        self.shutdown_complete = true;
        self.stop.request_stop();
        self.receiver.close();
        let batch = ShutdownBatch::new(
            self.drain_active_transfers(),
            self.drain_ready_transfers(),
            self.drain_buffered_transfers(),
        );
        final_results.extend(Self::cancel_batch(batch));
        Self::publish_released_results(final_results);
    }

    fn drain_buffered_transfers(&mut self) -> Vec<ScheduledTransfer> {
        let buffered = self
            .receiver
            .drain_available()
            .into_iter()
            .filter_map(|command| match command {
                OutboundCommand::Submit(transfer) => Some(*transfer),
                OutboundCommand::CancelStopped => None,
            })
            .collect();
        self.input_closed = self.receiver.is_closed();
        buffered
    }

    fn prepare_active_frame(&mut self) -> Option<ActiveFrameStep> {
        let runnable = self.active.as_mut()?;
        let class = runnable.class();
        let transfer = &mut runnable.item_mut().scheduled.transfer;
        if transfer.is_stopped() {
            return Some(ActiveFrameStep::Stopped);
        }
        let before_first_frame = transfer.is_before_first_frame();
        let generation = transfer.admitted.attempt().generation();
        let announced = &self.announced;
        let encoded = transfer.next_frame().and_then(|frame| {
            frame
                .map(|(payload, context)| {
                    announced
                        .encode(generation, payload.as_ref(), get_epoch_ms())
                        .map(|bytes| (bytes, context))
                })
                .transpose()
        });
        Some(match encoded {
            Ok(Some((bytes, context))) => ActiveFrameStep::Send {
                class,
                before_first_frame,
                bytes,
                context,
            },
            Ok(None) => ActiveFrameStep::Complete,
            Err(error) => ActiveFrameStep::Failed(error),
        })
    }

    async fn admit_active_frame(&mut self) {
        let Some(step) = self.prepare_active_frame() else {
            tracing::error!("outbound worker has no active transfer to admit");
            return;
        };
        match step {
            ActiveFrameStep::Stopped => {
                if let Some(runnable) = self.active.take() {
                    self.terminate_transfer(
                        runnable,
                        Ok(SendCompletionOutcome::Cancelled),
                        TerminationFairness::AdvanceFailedAttempt,
                    );
                }
            }
            ActiveFrameStep::Complete => {
                if let Some(runnable) = self.active.take() {
                    let useful_bytes = runnable.item().scheduled.transfer.useful_bytes();
                    self.terminate_transfer(
                        runnable,
                        Ok(SendCompletionOutcome::Succeeded),
                        TerminationFairness::AlreadyAdvanced,
                    );
                    self.measurements
                        .record(OutboundMeasurement::Sent { useful_bytes });
                }
            }
            ActiveFrameStep::Failed(error) => {
                if let Some(runnable) = self.active.take() {
                    self.fail_transfer(runnable, error, TerminationFairness::AdvanceFailedAttempt);
                }
            }
            ActiveFrameStep::Send {
                class,
                before_first_frame,
                bytes,
                context,
            } => {
                let Some(runnable) = self.active.as_ref() else {
                    tracing::error!("outbound worker lost its active transfer before send");
                    return;
                };
                let transfer = &runnable.item().scheduled.transfer;
                #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
                let _active_trace = test_trace::ActiveTransferGuard::enter(self.peer);
                let admission = send_data_with_timeout(
                    &transfer.admitted,
                    bytes,
                    &transfer.permit,
                    &transfer.stop,
                    transfer.detached_admission.as_ref(),
                    transfer.did,
                    context,
                )
                .await;
                if self.stop.is_stop_requested() {
                    let final_results =
                        self.finalize_stopped_active_admission(before_first_frame, admission);
                    self.shutdown_with_results(final_results);
                    return;
                }
                self.finish_active_admission(class, before_first_frame, admission);
            }
        }
    }

    fn finish_active_admission(
        &mut self,
        class: TransferClass,
        before_first_frame: bool,
        admission: ChunkSendProgress<Result<DeliveryFuture>>,
    ) {
        let Some(mut runnable) = self.active.take() else {
            tracing::error!("outbound worker lost its active transfer after send");
            return;
        };

        match admission {
            ChunkSendProgress::Ready(Ok(delivery)) => {
                if let Some(final_result) = runnable
                    .item_mut()
                    .scheduled
                    .transfer
                    .take_frame_admission_result()
                {
                    final_result.publish();
                }
                self.ready.record_frame_admitted(class);
                #[cfg(test)]
                test_trace::record(self.peer, class, runnable.item().id);
                let id = runnable.item().id;
                let delivery_wait =
                    Self::delivery_wait(id, class, delivery, &runnable.item().scheduled.transfer);
                self.ready.wait_for_delivery(id, runnable);
                self.deliveries.push(delivery_wait);
            }
            ChunkSendProgress::Ready(Err(error)) => {
                self.fail_transfer(runnable, error, TerminationFairness::AdvanceFailedAttempt);
            }
            ChunkSendProgress::Cancelled(reason) => {
                let record_failure = reason.records_peer_failure();
                let result = resolve_cancelled_transfer(before_first_frame, reason);
                self.terminate_transfer(
                    runnable,
                    result,
                    TerminationFairness::AdvanceFailedAttempt,
                );
                if record_failure {
                    self.measurements.record(OutboundMeasurement::FailedToSend);
                }
            }
        }
    }

    fn finalize_stopped_active_admission(
        &mut self,
        before_first_frame: bool,
        admission: ChunkSendProgress<Result<DeliveryFuture>>,
    ) -> Vec<FinalTransferResult> {
        let Some(runnable) = self.active.take() else {
            tracing::error!("outbound worker lost its stopped active transfer");
            return Vec::new();
        };
        let mut scheduled = runnable.into_parts().1.scheduled;
        let mut final_results = Vec::new();
        let result = match admission {
            ChunkSendProgress::Ready(Ok(delivery)) => {
                drop(delivery);
                final_results.extend(scheduled.transfer.take_frame_admission_result());
                Ok(SendCompletionOutcome::Cancelled)
            }
            ChunkSendProgress::Ready(Err(error)) => {
                if error.records_peer_send_failure() {
                    self.measurements.record(OutboundMeasurement::FailedToSend);
                }
                Err(error)
            }
            ChunkSendProgress::Cancelled(reason) => {
                if reason.records_peer_failure() {
                    self.measurements.record(OutboundMeasurement::FailedToSend);
                }
                resolve_cancelled_transfer(before_first_frame, reason)
            }
        };
        final_results.extend(Self::finalize_scheduled_transfer(scheduled, result));
        final_results
    }

    fn delivery_wait(
        id: u64,
        class: TransferClass,
        delivery: DeliveryFuture,
        transfer: &OutboundTransfer,
    ) -> DeliveryWaitFuture {
        let admitted = transfer.admitted.clone();
        let permit = transfer.permit.clone();
        let stop = transfer.stop.clone();
        let did = transfer.did;
        Box::pin(async move {
            let result = await_delivery_or_cancel(
                delivery,
                &admitted,
                &permit,
                &stop,
                did,
                "frame_delivery",
            )
            .await;
            DeliveryEvent { id, class, result }
        })
    }

    async fn wait_for_input(&mut self) {
        if self.deliveries.is_empty() {
            match self.receiver.next().await {
                Some(command) => self.handle_commands([command]),
                None => self.input_closed = true,
            }
            return;
        }

        enum WorkerInput {
            Command(Option<OutboundCommand>),
            Delivery(Option<DeliveryEvent>),
        }

        let input = {
            let command = self.receiver.next().fuse();
            let delivery = self.deliveries.next().fuse();
            pin_mut!(command, delivery);
            select! {
                command = command => WorkerInput::Command(command),
                delivery = delivery => WorkerInput::Delivery(delivery),
            }
        };
        match input {
            WorkerInput::Command(Some(command)) => self.handle_commands([command]),
            WorkerInput::Command(None) => self.input_closed = true,
            WorkerInput::Delivery(Some(event)) => self.handle_delivery(event),
            WorkerInput::Delivery(None) => {}
        }
    }
}

impl Drop for OutboundWorker {
    fn drop(&mut self) {
        // Task panic and runtime cancellation both pass through this state:
        // close ingress, drain permit-bearing commands, and publish completion
        // only after their capacity guards have been released.
        self.shutdown();
    }
}

#[cfg(all(test, any(feature = "dummy", target_family = "wasm")))]
mod test_cancellation;
#[cfg(test)]
mod test_outbound;
