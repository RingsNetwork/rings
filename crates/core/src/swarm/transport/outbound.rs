//! Per-peer outbound transfer scheduling.
//!
//! Each message class owns one FIFO lane whose head is either runnable or
//! waiting for frame delivery. A lane never admits a second transfer before
//! its current transfer finishes, so messages in the same class cannot
//! interleave. Runnable lane heads are selected with bounded DHT-control
//! priority and round-robin service for storage, E2E, and application traffic.
//! Cross-class submission order is intentionally not preserved: every protocol
//! sequence that requires ordering must keep all of its variants in one class.
//! The worker admits at most one frame before it drains completions and chooses
//! a lane again, which is the preemption boundary.
//!
//! A peer admits at most [`OUTBOUND_TRANSFER_QUEUE_CAPACITY`] transfers across
//! the command channel, lane queues, and delivery waits. Shutdown closes the
//! command channel synchronously; the worker then cancels every admitted
//! transfer and drops outstanding delivery futures.

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use std::cell::Cell;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::Weak;

use bytes::Bytes;
use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use rings_transport::delivery::DeliveryFuture;

use super::delivery::await_delivery_or_cancel;
use super::delivery::frame_chunk;
use super::delivery::send_data_with_timeout;
use super::delivery::ChunkSendPermit;
use super::delivery::ChunkSendProgress;
use super::delivery::SendCompletionOutcome;
use super::delivery::TransferStop;
use super::AdmittedConnection;
use crate::chunk::Chunk;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::lifecycle::StopSource;
use crate::lifecycle::StopToken;
use crate::measure::MeasureImpl;
use crate::session::SessionSk;

#[path = "outbound/capacity.rs"]
mod capacity;
#[path = "outbound/measurement.rs"]
mod measurement;
#[path = "outbound/model.rs"]
mod model;
#[path = "outbound/queue.rs"]
mod queue;

use capacity::GlobalTransferCapacity;
use capacity::TransferCapacity;
pub(super) use capacity::TransferCapacityPermit;
#[cfg(test)]
pub(crate) use capacity::OUTBOUND_CONTROL_RESERVED_TRANSFERS;
#[cfg(test)]
pub(crate) use capacity::OUTBOUND_DATA_TRANSFER_CAPACITY;
pub(crate) use capacity::OUTBOUND_TRANSFER_QUEUE_CAPACITY;
use measurement::MeasurementReceiver;
use measurement::MeasurementRecorder;
use measurement::OutboundMeasurement;
pub(super) use model::OutboundCompletion;
pub(super) use model::OutboundMessageMeta;
use model::TransferClass;
use queue::RunnableTransfer;
use queue::TransferQueues;
#[cfg(test)]
use queue::OUTBOUND_CONTROL_BURST;

/// Maximum commands consumed before runnable lane heads are reconsidered.
const OUTBOUND_COMMAND_DRAIN_BUDGET: usize = 32;

type TransferResultSender = oneshot::Sender<Result<SendCompletionOutcome>>;

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
thread_local! {
    static OUTBOUND_SUBMIT_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn reset_outbound_submit_count_for_test() {
    OUTBOUND_SUBMIT_COUNT.with(|count| count.set(0));
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn outbound_submit_count_for_test() -> usize {
    OUTBOUND_SUBMIT_COUNT.with(Cell::get)
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
fn record_outbound_submit_for_test() {
    OUTBOUND_SUBMIT_COUNT.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(super) type ChunkFrames = Box<dyn Iterator<Item = Chunk> + Send>;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(super) type ChunkFrames = Box<dyn Iterator<Item = Chunk>>;

enum FrameSource {
    Whole(Option<Bytes>),
    Chunked {
        session_sk: SessionSk,
        did: Did,
        chunks: ChunkFrames,
    },
}

impl FrameSource {
    fn next_frame(&mut self) -> Result<Option<(Bytes, &'static str)>> {
        match self {
            Self::Whole(frame) => Ok(frame.take().map(|bytes| (bytes, "whole_message"))),
            Self::Chunked {
                session_sk,
                did,
                chunks,
            } => {
                let Some(chunk) = chunks.next() else {
                    return Ok(None);
                };
                let [position, _total] = chunk.chunk;
                let context = if position == 0 {
                    "chunked_first"
                } else {
                    "chunked_tail"
                };
                frame_chunk(session_sk, *did, chunk).map(|bytes| Some((bytes, context)))
            }
        }
    }
}

pub(super) struct OutboundTransfer {
    class: TransferClass,
    did: Did,
    admitted: AdmittedConnection,
    permit: ChunkSendPermit,
    source: FrameSource,
    completion: TransferCompletion,
    first_frame_admitted: bool,
    stop: TransferStop,
}

struct TransferCompletion {
    policy: OutboundCompletion,
    sender: Option<TransferResultSender>,
}

struct FinalTransferResult {
    sender: TransferResultSender,
    result: Result<SendCompletionOutcome>,
}

struct ShutdownBatch<T> {
    ready: Vec<T>,
    buffered: Vec<T>,
}

impl<T> ShutdownBatch<T> {
    fn new(ready: Vec<T>, buffered: Vec<T>) -> Self {
        Self { ready, buffered }
    }

    fn finalize<R>(self, finalize: impl FnMut(T) -> Option<R>) -> Vec<R> {
        self.ready
            .into_iter()
            .chain(self.buffered)
            .filter_map(finalize)
            .collect()
    }
}

impl FinalTransferResult {
    /// Publish only after the scheduled transfer has dropped its capacity permit.
    fn publish(self) {
        let _ = self.sender.send(self.result);
    }
}

impl TransferCompletion {
    fn resolve_first_admission(&mut self) {
        if self.policy == OutboundCompletion::Detached {
            self.send(Ok(SendCompletionOutcome::Succeeded));
        }
    }

    fn take_final(&mut self, result: Result<SendCompletionOutcome>) -> Option<FinalTransferResult> {
        self.sender
            .take()
            .map(|sender| FinalTransferResult { sender, result })
    }

    fn send(&mut self, result: Result<SendCompletionOutcome>) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(result);
        }
    }
}

pub(super) struct OutboundTransferRoute {
    class: TransferClass,
    did: Did,
    admitted: AdmittedConnection,
    permit: ChunkSendPermit,
}

impl OutboundTransferRoute {
    pub(super) fn new(
        class: TransferClass,
        did: Did,
        admitted: AdmittedConnection,
        permit: ChunkSendPermit,
    ) -> Self {
        Self {
            class,
            did,
            admitted,
            permit,
        }
    }
}

impl OutboundTransfer {
    pub(super) fn whole(
        route: OutboundTransferRoute,
        data: Bytes,
        completion: OutboundCompletion,
        stop: StopToken,
    ) -> (Self, oneshot::Receiver<Result<SendCompletionOutcome>>) {
        Self::new(route, FrameSource::Whole(Some(data)), completion, stop)
    }

    pub(super) fn chunked(
        route: OutboundTransferRoute,
        session_sk: SessionSk,
        chunks: ChunkFrames,
        completion: OutboundCompletion,
        stop: StopToken,
    ) -> (Self, oneshot::Receiver<Result<SendCompletionOutcome>>) {
        let did = route.did;
        Self::new(
            route,
            FrameSource::Chunked {
                session_sk,
                did,
                chunks,
            },
            completion,
            stop,
        )
    }

    fn new(
        route: OutboundTransferRoute,
        source: FrameSource,
        completion: OutboundCompletion,
        stop: StopToken,
    ) -> (Self, oneshot::Receiver<Result<SendCompletionOutcome>>) {
        let (sender, receiver) = oneshot::channel();
        let completion = TransferCompletion {
            policy: completion,
            sender: Some(sender),
        };
        (
            Self {
                class: route.class,
                did: route.did,
                admitted: route.admitted,
                permit: route.permit,
                source,
                completion,
                first_frame_admitted: false,
                stop: TransferStop::new(stop),
            },
            receiver,
        )
    }

    fn class(&self) -> TransferClass {
        self.class
    }

    fn next_frame(&mut self) -> Result<Option<(Bytes, &'static str)>> {
        self.source.next_frame()
    }

    fn is_before_first_frame(&self) -> bool {
        !self.first_frame_admitted
    }

    fn is_stopped(&self) -> bool {
        self.stop.should_stop()
    }

    fn bind_scheduler_stop(&mut self, stop: StopToken) {
        self.stop.bind_scheduler(stop);
    }

    fn mark_frame_admitted(&mut self) {
        if !self.first_frame_admitted {
            self.first_frame_admitted = true;
            self.completion.resolve_first_admission();
        }
    }

    fn take_final(&mut self, result: Result<SendCompletionOutcome>) -> Option<FinalTransferResult> {
        self.completion.take_final(result)
    }
}

struct ScheduledTransfer {
    transfer: OutboundTransfer,
    _capacity_permit: TransferCapacityPermit,
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

#[derive(Clone)]
pub(super) struct OutboundPeerHandle {
    peer: Did,
    state: Arc<OutboundPeerState>,
}

struct OutboundPeerState {
    sender: Mutex<mpsc::Sender<Box<ScheduledTransfer>>>,
    _capacity: Arc<TransferCapacity>,
    stop: StopSource,
}

impl OutboundPeerHandle {
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(super) fn reserve(
        &self,
        class: TransferClass,
        bytes: usize,
    ) -> Result<TransferCapacityPermit> {
        if self.state.stop.is_stop_requested() {
            return Err(Error::ChannelSendMessageFailed);
        }
        self.state._capacity.try_acquire(self.peer, class, bytes)
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
        let scheduled = ScheduledTransfer {
            transfer,
            _capacity_permit: capacity_permit,
        };
        let mut sender = self
            .state
            .sender
            .lock()
            .map_err(|_| Error::ChannelSendMessageFailed)?;
        if self.state.stop.is_stop_requested() {
            return Err(Error::ChannelSendMessageFailed);
        }
        let result = sender.try_send(Box::new(scheduled)).map_err(|error| {
            if error.is_full() {
                Error::OutboundTransferCapacityExceeded {
                    peer: self.peer,
                    capacity: OUTBOUND_TRANSFER_QUEUE_CAPACITY,
                }
            } else {
                Error::ChannelSendMessageFailed
            }
        });
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        if result.is_ok() {
            record_outbound_submit_for_test();
        }
        result
    }

    fn shutdown(&self) {
        self.state.shutdown();
    }
}

impl OutboundPeerState {
    fn shutdown(&self) {
        self.stop.request_stop();
        if let Ok(mut sender) = self.sender.lock() {
            sender.close_channel();
        }
    }
}

impl Drop for OutboundPeerState {
    fn drop(&mut self) {
        self.stop.request_stop();
        match self.sender.get_mut() {
            Ok(sender) => sender.close_channel(),
            Err(poisoned) => poisoned.into_inner().close_channel(),
        }
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
        if let Some(handle) = registry.peers.get(&peer) {
            return Ok(handle.clone());
        }
        let capacity = registry.capacity(peer, &self.global_capacity);
        let (sender, receiver) = mpsc::channel(OUTBOUND_TRANSFER_QUEUE_CAPACITY);
        let stop = StopSource::new();
        let state = Arc::new(OutboundPeerState {
            sender: Mutex::new(sender),
            _capacity: capacity,
            stop: stop.clone(),
        });
        let handle = OutboundPeerHandle { peer, state };
        let (measurements, measurement_receiver) =
            MeasurementRecorder::channel(self.measure.clone(), peer);
        spawn_worker(
            OutboundWorker::new(receiver, stop.token(), measurements),
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

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
impl super::SwarmTransport {
    pub(crate) fn outbound_admitted_transfer_count_for_test(&self, peer: Did) -> Option<usize> {
        self.outbound_schedulers
            .admitted_transfer_count_for_test(peer)
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
impl super::SwarmTransport {
    pub(crate) fn outbound_admitted_transfer_total_for_test(&self) -> usize {
        self.outbound_schedulers.admitted_transfer_total_for_test()
    }
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
type DeliveryWaitFuture = Pin<Box<dyn Future<Output = DeliveryEvent> + Send>>;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
type DeliveryWaitFuture = Pin<Box<dyn Future<Output = DeliveryEvent>>>;

struct OutboundWorker {
    receiver: mpsc::Receiver<Box<ScheduledTransfer>>,
    ready: TransferQueues<QueuedTransfer>,
    deliveries: FuturesUnordered<DeliveryWaitFuture>,
    measurements: MeasurementRecorder,
    stop: StopToken,
    next_id: u64,
    input_closed: bool,
}

impl OutboundWorker {
    fn new(
        receiver: mpsc::Receiver<Box<ScheduledTransfer>>,
        stop: StopToken,
        measurements: MeasurementRecorder,
    ) -> Self {
        Self {
            receiver,
            ready: TransferQueues::default(),
            deliveries: FuturesUnordered::new(),
            measurements,
            stop,
            next_id: 0,
            input_closed: false,
        }
    }

    async fn run(mut self) {
        loop {
            if self.stop.should_stop() {
                self.shutdown();
                return;
            }
            self.drain_available();
            if self.stop.should_stop() {
                self.shutdown();
                return;
            }
            if self.input_closed {
                let final_results = Self::cancel_batch(ShutdownBatch::new(
                    self.drain_ready_transfers(),
                    Vec::new(),
                ));
                Self::publish_released_results(final_results);
                return;
            }
            if let Some(transfer) = self.ready.pop() {
                self.admit_one_frame(transfer).await;
                continue;
            }
            self.wait_for_input().await;
        }
    }

    fn drain_available(&mut self) {
        for _ in 0..OUTBOUND_COMMAND_DRAIN_BUDGET {
            match self.receiver.next().now_or_never() {
                Some(Some(transfer)) => self.enqueue_transfer(*transfer),
                Some(None) => {
                    self.input_closed = true;
                    break;
                }
                None => break,
            }
        }

        for _ in 0..OUTBOUND_COMMAND_DRAIN_BUDGET {
            match self.deliveries.next().now_or_never() {
                Some(Some(event)) => self.handle_delivery(event),
                Some(None) | None => break,
            }
        }
    }

    fn enqueue_transfer(&mut self, scheduled: ScheduledTransfer) {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let class = scheduled.transfer.class();
        self.ready.push(class, QueuedTransfer { id, scheduled });
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

    fn shutdown(&mut self) {
        self.receiver.close();
        let batch = ShutdownBatch::new(
            self.drain_ready_transfers(),
            self.drain_buffered_transfers(),
        );
        let final_results = Self::cancel_batch(batch);
        Self::publish_released_results(final_results);
    }

    fn drain_buffered_transfers(&mut self) -> Vec<ScheduledTransfer> {
        let mut buffered = Vec::new();
        loop {
            match self.receiver.next().now_or_never() {
                Some(Some(transfer)) => buffered.push(*transfer),
                Some(None) => {
                    self.input_closed = true;
                    break;
                }
                None => break,
            }
        }
        buffered
    }

    async fn admit_one_frame(&mut self, mut runnable: RunnableTransfer<QueuedTransfer>) {
        let class = runnable.class();
        if runnable.item().scheduled.transfer.is_stopped() {
            self.terminate_transfer(
                runnable,
                Ok(SendCompletionOutcome::Cancelled),
                TerminationFairness::AdvanceFailedAttempt,
            );
            return;
        }
        let before_first_frame = runnable.item().scheduled.transfer.is_before_first_frame();
        let frame = match runnable.item_mut().scheduled.transfer.next_frame() {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                self.terminate_transfer(
                    runnable,
                    Ok(SendCompletionOutcome::Succeeded),
                    TerminationFairness::AlreadyAdvanced,
                );
                self.measurements.record(OutboundMeasurement::Sent);
                return;
            }
            Err(error) => {
                self.fail_transfer(runnable, error, TerminationFairness::AdvanceFailedAttempt);
                return;
            }
        };
        let (bytes, context) = frame;
        let admission = send_data_with_timeout(
            &runnable.item().scheduled.transfer.admitted,
            bytes,
            &runnable.item().scheduled.transfer.permit,
            &runnable.item().scheduled.transfer.stop,
            runnable.item().scheduled.transfer.did,
            context,
        )
        .await;

        match admission {
            ChunkSendProgress::Ready(Ok(delivery)) => {
                runnable.item_mut().scheduled.transfer.mark_frame_admitted();
                self.ready.record_frame_admitted(class);
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
                let result = if before_first_frame {
                    match reason.resolve_initial() {
                        Ok(()) => Ok(SendCompletionOutcome::Cancelled),
                        Err(error) => Err(error),
                    }
                } else {
                    Ok(SendCompletionOutcome::Cancelled)
                };
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
                Some(transfer) => self.enqueue_transfer(*transfer),
                None => self.input_closed = true,
            }
            return;
        }

        enum WorkerInput {
            Command(Option<Box<ScheduledTransfer>>),
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
            WorkerInput::Command(Some(transfer)) => self.enqueue_transfer(*transfer),
            WorkerInput::Command(None) => self.input_closed = true,
            WorkerInput::Delivery(Some(event)) => self.handle_delivery(event),
            WorkerInput::Delivery(None) => {}
        }
    }
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
fn spawn_worker(worker: OutboundWorker, measurements: MeasurementReceiver) -> Result<()> {
    wasm_bindgen_futures::spawn_local(worker.run());
    wasm_bindgen_futures::spawn_local(measurements.run());
    Ok(())
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
fn spawn_worker(worker: OutboundWorker, measurements: MeasurementReceiver) -> Result<()> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| Error::OutboundSchedulerRuntimeUnavailable)?;
    runtime.spawn(worker.run());
    runtime.spawn(measurements.run());
    Ok(())
}

#[cfg(test)]
#[path = "outbound/tests.rs"]
mod tests;
