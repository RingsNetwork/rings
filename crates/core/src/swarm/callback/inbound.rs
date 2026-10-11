//! Bounded inbound admission and per-lane actor scheduling.
//!
//! Transport first bounds undecoded frames by per-lane credit and transfers an opaque
//! credit lease. Core then verifies the frame and reserves per-peer plus class-aware actor
//! capacity, and only then takes its lane ticket. The transport lease is released after the
//! ticket, when both capacity ownership and lane order are established, which returns the
//! sender's credit. Decode, validation, reassembly, and dispatch retain only the core permit.
//!
//! Capacity pressure parks the arrival instead of refusing it: the arrival keeps its lease, so
//! the pressure reaches the sender as transport backpressure. Parked arrivals are admitted in
//! resource order (`capacity::InboundResource`). The wait cannot deadlock against the actor that
//! releases capacity, because a waiting arrival holds no ticket: the actor never orders its
//! lanes behind it, and releases capacity as the events it already admitted complete.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::stream::FuturesUnordered;
use futures::FutureExt;
use futures::StreamExt;

use super::logical::edge_relation;
use super::InboundFrameLease;
use super::InboundProcessor;
use super::LogicalInbound;
use super::PreparedInboundFrame;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::measure::Authentication;
use crate::message::EdgeRelation;
use crate::message::MessagePayload;
use crate::utils::try_sleep;
use crate::utils::GenerationWitness;
use crate::utils::Instant;

mod barrier;
mod capacity;
mod lane;
mod reassembly;
mod reassembly_clock;
mod spawn;
mod ticket;

use self::barrier::lane_waits_for_reassembly;
use self::barrier::ReassemblyHandoffBarrier;
use self::capacity::memory_reservation;
pub(crate) use self::capacity::InboundCapacity;
use self::capacity::InboundCapacityPermit;
pub(crate) use self::lane::InboundLane;
use self::lane::INBOUND_LANE_COUNT;
use self::reassembly::process_chunk_event;
pub(super) use self::reassembly_clock::ReassemblyClock;
use self::spawn::spawn_actor;
use self::ticket::InboundCommand;
use self::ticket::InboundSender;
use self::ticket::InboundTicket;

macro_rules! inbound_lane_entry {
    ($lanes:expr, $lane:expr) => {{
        let [control, storage, e2e, application, reassembly] = $lanes;
        match $lane {
            InboundLane::DhtControl => control,
            InboundLane::Storage => storage,
            InboundLane::E2e => e2e,
            InboundLane::Application => application,
            InboundLane::Reassembly => reassembly,
        }
    }};
}

/// Total logical events retained across queued and executing actor work.
const INBOUND_MAILBOX_CAPACITY: usize = 256;
/// Weighted decoded representations retained by the actor.
const INBOUND_MAILBOX_BYTE_CAPACITY: usize = 256 * 1024 * 1024;
/// Per-lane slots preserved when other lanes borrow shared capacity.
const INBOUND_RESERVED_TRANSFERS_PER_LANE: usize = 16;
/// One maximum transport frame fits each lane's byte reservation after decoding.
const INBOUND_RESERVED_BYTES_PER_LANE: usize = 1024 * 1024;
const INBOUND_RESERVED_TRANSFERS: [usize; INBOUND_LANE_COUNT] =
    [INBOUND_RESERVED_TRANSFERS_PER_LANE; INBOUND_LANE_COUNT];
const INBOUND_RESERVED_BYTES: [usize; INBOUND_LANE_COUNT] =
    [INBOUND_RESERVED_BYTES_PER_LANE; INBOUND_LANE_COUNT];
/// Per-peer share prevents one connection from occupying the node-wide actor.
const INBOUND_PEER_CAPACITY: usize = 32;
const INBOUND_PEER_BYTE_CAPACITY: usize = 128 * 1024 * 1024;
/// Bounds command work before completed lane tasks are observed.
const INBOUND_COMMAND_DRAIN_BUDGET: usize = 32;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
const REASSEMBLY_CLEANUP_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(not(all(test, feature = "dummy", not(target_family = "wasm"))))]
const REASSEMBLY_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
type InboundReply = oneshot::Sender<Result<()>>;

enum InboundCompletion {
    Await(InboundReply),
    LogHeld { peer: Did },
}

struct InboundEvent {
    sequence: u64,
    peer: Option<Did>,
    authentication: Authentication,
    payload: MessagePayload,
    prepared_message: Option<crate::message::Message>,
    lane: InboundLane,
    wire_bytes: usize,
    permit: InboundCapacityPermit,
    completion: InboundCompletion,
}

pub(super) struct InboundSubmission {
    peer: Option<Did>,
    authentication: Authentication,
    prepared: PreparedInboundFrame,
    lease: InboundFrameLease,
}

impl InboundSubmission {
    pub(super) const fn new(
        peer: Option<Did>,
        authentication: Authentication,
        lease: InboundFrameLease,
        prepared: PreparedInboundFrame,
    ) -> Self {
        Self {
            peer,
            authentication,
            prepared,
            lease,
        }
    }
}

impl InboundEvent {
    const fn lane(&self) -> InboundLane {
        self.lane
    }
}

#[derive(Clone)]
pub(super) struct InboundMailbox {
    sender: Arc<Mutex<InboundSender>>,
    capacity: Arc<InboundCapacity>,
    actor_available: bool,
    /// Bumped after each raw transport lease is released at the core handoff.
    handoffs: Arc<GenerationWitness>,
    /// Bumped by the actor after each completed reassembly cleanup pass,
    /// periodic or close-time; the mailbox only reads it for tests.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    cleanup_passes: Arc<GenerationWitness>,
}

impl InboundMailbox {
    pub(super) fn spawn(
        processor: InboundProcessor,
        capacity: Arc<InboundCapacity>,
        reassembly_clock: ReassemblyClock,
    ) -> Self {
        let (sender, receiver) = mpsc::unbounded();
        let cleanup_passes = Arc::new(GenerationWitness::default());
        let actor = InboundActor::new(
            processor,
            receiver,
            reassembly_clock,
            Arc::clone(&cleanup_passes),
        );
        let actor_available = spawn_actor(actor);
        Self {
            sender: Arc::new(Mutex::new(InboundSender::new(sender))),
            capacity,
            actor_available,
            handoffs: Arc::new(GenerationWitness::default()),
            #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
            cleanup_passes,
        }
    }

    pub(super) async fn submit_prepared(
        &self,
        processor: &InboundProcessor,
        submission: InboundSubmission,
    ) -> Result<()> {
        self.ensure_actor_available()?;
        let (reply, completion) = oneshot::channel();
        self.enqueue_to_lane(processor, submission, InboundCompletion::Await(reply))
            .await?;
        match completion.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::InboundMailboxClosed),
        }
    }

    pub(super) async fn submit_prepared_detached(
        &self,
        processor: &InboundProcessor,
        peer: Did,
        submission: InboundSubmission,
    ) -> Result<()> {
        self.ensure_actor_available()?;
        self.enqueue_to_lane(processor, submission, InboundCompletion::LogHeld { peer })
            .await
    }

    fn ensure_actor_available(&self) -> Result<()> {
        if !self.actor_available {
            return Err(Error::InboundMailboxRuntimeUnavailable);
        }
        if self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_closed()
        {
            return Err(Error::InboundMailboxClosed);
        }
        Ok(())
    }

    async fn enqueue_to_lane(
        &self,
        processor: &InboundProcessor,
        submission: InboundSubmission,
        completion: InboundCompletion,
    ) -> Result<()> {
        let InboundSubmission {
            peer,
            authentication,
            prepared,
            lease:
                InboundFrameLease {
                    bytes,
                    transport_credit,
                    held: _,
                    #[cfg(test)]
                    in_flight,
                },
        } = submission;
        let lane = prepared.lane;
        if !prepared.kind.is_chunk() {
            if !processor.pending_connection_admits(peer).await? {
                finish_completion(completion, Ok(()));
                return Ok(());
            }
            let edge = edge_relation(peer, authentication, &prepared.payload);
            processor
                .logical
                .admit_final_transaction(&prepared.payload, &prepared.message, lane, edge)
                .await?;
        }
        // Capacity before the ticket: an arrival waiting for capacity holds no sequence, so
        // the actor's ordering never waits on it.
        let permit = self
            .capacity
            .acquire(peer, lane, memory_reservation(bytes.len()))
            .await?;
        let ticket = self.reserve_ticket(lane)?;
        let PreparedInboundFrame {
            payload,
            message,
            kind,
            ..
        } = prepared;
        // Core now owns both retained decoded representations. Release the raw
        // bytes and their transport lease together at this handoff boundary.
        let wire_bytes = bytes.len();
        drop((bytes, transport_credit));
        // Test builds: the actor's capacity permit now covers the frame.
        #[cfg(test)]
        drop(in_flight);
        self.handoffs.bump();
        if !processor.pending_connection_admits(peer).await? {
            finish_completion(completion, Ok(()));
            return Ok(());
        }
        let payload = if kind.is_chunk() {
            processor
                .validate_preverified_payload(peer, authentication, &payload)
                .await?;
            payload
        } else {
            processor
                .accept_verified_logical_message(peer, authentication, payload)
                .await?
        };
        let sequence = ticket.sequence();
        ticket.commit(InboundEvent {
            sequence,
            peer,
            authentication,
            payload,
            prepared_message: Some(message),
            lane,
            wire_bytes,
            permit,
            completion,
        })?;
        Ok(())
    }

    fn reserve_ticket(&self, lane: InboundLane) -> Result<InboundTicket> {
        self.sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reserve(lane)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn admitted_count_for_test(&self) -> usize {
        self.capacity.admitted_count_for_test()
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) async fn await_admitted_count_for_test(&self, predicate: impl Fn(usize) -> bool) {
        self.capacity.await_admitted_count_for_test(predicate).await;
    }

    /// Wait until the number of arrivals waiting for the mailbox satisfies `predicate`.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) async fn await_waiting_for_test(&self, predicate: impl Fn(usize) -> bool) {
        self.capacity.await_waiting_for_test(predicate).await;
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) async fn await_handoffs_for_test(&self, predicate: impl Fn(u64) -> bool) {
        self.handoffs.await_until(predicate).await;
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn cleanup_passes_for_test(&self) -> u64 {
        self.cleanup_passes.get()
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) async fn await_cleanup_passes_for_test(&self, predicate: impl Fn(u64) -> bool) {
        self.cleanup_passes.await_until(predicate).await;
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn hold_application_admission_for_test(&self) -> Result<impl Drop> {
        self.reserve_ticket(InboundLane::Application)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn hold_application_capacity_for_test(
        &self,
        peer: crate::dht::Did,
    ) -> Result<impl Drop> {
        self.capacity
            .try_acquire(Some(peer), InboundLane::Application, 1)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn close_for_test(&self) {
        self.sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .close_channel();
    }
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(super) const fn application_capacity_for_test() -> usize {
    INBOUND_MAILBOX_CAPACITY - INBOUND_RESERVED_TRANSFERS_PER_LANE * (INBOUND_LANE_COUNT - 1)
}

/// The frames one peer may have in flight: the bound an admitted peer and a peer awaiting
/// admission share.
pub(super) const fn peer_capacity() -> usize {
    INBOUND_PEER_CAPACITY
}

struct InboundQueues {
    lanes: [VecDeque<InboundQueueEntry>; INBOUND_LANE_COUNT],
}

struct InboundQueueEntry {
    sequence: u64,
    event: Option<InboundEvent>,
}

impl InboundQueues {
    fn new() -> Self {
        Self {
            lanes: std::array::from_fn(|_| VecDeque::new()),
        }
    }

    fn push_pending(&mut self, sequence: u64, lane: InboundLane) {
        let queue = inbound_lane_entry!(&mut self.lanes, lane);
        debug_assert!(queue.back().is_none_or(|entry| entry.sequence < sequence));
        queue.push_back(InboundQueueEntry {
            sequence,
            event: None,
        });
    }

    fn push_ready(&mut self, event: InboundEvent) {
        let queue = inbound_lane_entry!(&mut self.lanes, event.lane());
        if let Some(entry) = queue
            .iter_mut()
            .find(|entry| entry.sequence == event.sequence)
        {
            entry.event = Some(event);
            return;
        }
        let entry = InboundQueueEntry {
            sequence: event.sequence,
            event: Some(event),
        };
        let position = queue.partition_point(|queued| queued.sequence < entry.sequence);
        queue.insert(position, entry);
    }

    fn cancel(&mut self, sequence: u64, lane: InboundLane) {
        let queue = inbound_lane_entry!(&mut self.lanes, lane);
        if let Some(index) = queue.iter().position(|entry| entry.sequence == sequence) {
            queue.remove(index);
        }
    }

    fn pop(&mut self, lane: InboundLane) -> Option<InboundEvent> {
        let queue = inbound_lane_entry!(&mut self.lanes, lane);
        queue.front()?.event.as_ref()?;
        queue.pop_front()?.event
    }

    fn front_sequence(&self, lane: InboundLane) -> Option<u64> {
        inbound_lane_entry!(&self.lanes, lane)
            .front()
            .map(|entry| entry.sequence)
    }
}

struct InboundTaskCompletion {
    lane: InboundLane,
    sequence: u64,
    next: Option<InboundEvent>,
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
type InboundTaskFuture = Pin<Box<dyn Future<Output = InboundTaskCompletion> + Send>>;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
type InboundTaskFuture = Pin<Box<dyn Future<Output = InboundTaskCompletion>>>;

struct InboundActor {
    processor: InboundProcessor,
    receiver: mpsc::UnboundedReceiver<InboundCommand>,
    queues: InboundQueues,
    active: FuturesUnordered<InboundTaskFuture>,
    active_lanes: [Option<u64>; INBOUND_LANE_COUNT],
    reassembly_handoff_barrier: Option<ReassemblyHandoffBarrier>,
    reassembly_clock: ReassemblyClock,
    next_reassembly_cleanup: Instant,
    input_closed: bool,
    cleanup_passes: Arc<GenerationWitness>,
}

impl InboundActor {
    fn new(
        processor: InboundProcessor,
        receiver: mpsc::UnboundedReceiver<InboundCommand>,
        reassembly_clock: ReassemblyClock,
        cleanup_passes: Arc<GenerationWitness>,
    ) -> Self {
        Self {
            processor,
            receiver,
            queues: InboundQueues::new(),
            active: FuturesUnordered::new(),
            active_lanes: [None; INBOUND_LANE_COUNT],
            reassembly_handoff_barrier: None,
            reassembly_clock,
            next_reassembly_cleanup: Instant::now() + REASSEMBLY_CLEANUP_INTERVAL,
            input_closed: false,
            cleanup_passes,
        }
    }

    async fn run(mut self) {
        loop {
            if self.reassembly_cleanup_delay().is_zero() {
                self.cleanup_expired_reassembly().await;
            }
            self.release_started_reassembly_handoff();
            self.drain_available();
            if self.input_closed {
                self.finish_reassembly_after_close().await;
                return;
            }
            self.dispatch_runnable();
            if self.active.is_empty() {
                if !self.wait_for_input().await {
                    return;
                }
                continue;
            }
            let cleanup_delay = self.reassembly_cleanup_delay();
            let input = self.receiver.next().fuse();
            let completed = self.active.next().fuse();
            let cleanup = try_sleep(cleanup_delay).fuse();
            futures::pin_mut!(input, completed, cleanup);
            futures::select! {
                event = input => self.handle_input(event),
                completion = completed => {
                    if let Some(completion) = completion {
                        self.handle_completion(completion);
                    }
                },
                completed = cleanup => {
                    if !self.handle_reassembly_cleanup_timer(completed).await {
                        return;
                    }
                },
            }
        }
    }

    fn drain_available(&mut self) {
        for _ in 0..INBOUND_COMMAND_DRAIN_BUDGET {
            match self.receiver.try_recv() {
                Ok(command) => self.handle_command(command),
                Err(error) if error.is_closed() => {
                    self.input_closed = true;
                    return;
                }
                Err(_) => return,
            }
        }
    }

    fn dispatch_runnable(&mut self) {
        let reassembly_barrier = self.reassembly_barrier_sequence();
        for (lane, active_lane) in InboundLane::ALL
            .into_iter()
            .zip(self.active_lanes.iter_mut())
        {
            if active_lane.is_some() {
                continue;
            }
            let Some(sequence) = self.queues.front_sequence(lane) else {
                continue;
            };
            if self
                .reassembly_handoff_barrier
                .as_ref()
                .is_some_and(|barrier| barrier.blocks(lane))
            {
                continue;
            }
            // The nested class is unknown until reassembly completes. Preserve data-plane order
            // across that handoff, while keeping control traffic independent for liveness.
            let waits_for_reassembly = lane_waits_for_reassembly(lane)
                && reassembly_barrier.is_some_and(|barrier| sequence > barrier);
            if waits_for_reassembly {
                #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
                if lane == InboundLane::from_class(crate::message::MessageCategory::DhtControl) {
                    crate::simulation::record_barrier_control_blocked();
                }
                continue;
            }
            let Some(event) = self.queues.pop(lane) else {
                continue;
            };
            let handoff_started = self
                .reassembly_handoff_barrier
                .as_ref()
                .filter(|barrier| barrier.sequence == sequence)
                .map(ReassemblyHandoffBarrier::start_marker);
            *active_lane = Some(sequence);
            self.active.push(Box::pin(process_event(
                self.processor.clone(),
                event,
                handoff_started.clone(),
            )));
            if handoff_started.is_some() {
                break;
            }
        }
    }

    fn reassembly_barrier_sequence(&self) -> Option<u64> {
        [
            *inbound_lane_entry!(&self.active_lanes, InboundLane::Reassembly),
            self.queues.front_sequence(InboundLane::Reassembly),
            self.reassembly_handoff_barrier
                .as_ref()
                .map(|barrier| barrier.sequence),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    async fn wait_for_input(&mut self) -> bool {
        let cleanup_delay = self.reassembly_cleanup_delay();
        let input = self.receiver.next().fuse();
        let cleanup = try_sleep(cleanup_delay).fuse();
        futures::pin_mut!(input, cleanup);
        futures::select! {
            event = input => {
                self.handle_input(event);
                true
            },
            completed = cleanup => self.handle_reassembly_cleanup_timer(completed).await,
        }
    }

    async fn handle_reassembly_cleanup_timer(&mut self, completed: bool) -> bool {
        if completed {
            self.cleanup_expired_reassembly().await;
            return true;
        }
        tracing::error!("stopping inbound actor after reassembly timer scheduling failed");
        self.processor
            .discard_reassembly_after_close_timer_failure()
            .await;
        false
    }

    fn reassembly_cleanup_delay(&self) -> Duration {
        self.next_reassembly_cleanup
            .saturating_duration_since(Instant::now())
    }

    async fn cleanup_expired_reassembly(&mut self) {
        let now_ms = self.reassembly_clock.now_ms();
        self.processor.remove_expired_reassembly_at(now_ms).await;
        self.processor.sweep_session_hold_at(now_ms).await;
        self.next_reassembly_cleanup = Instant::now() + REASSEMBLY_CLEANUP_INTERVAL;
        self.cleanup_passes.bump();
    }

    async fn finish_reassembly_after_close(&mut self) {
        // Preserve the old shutdown contract for queued/application work: all
        // unfinished futures and their permits are cancelled immediately.
        // Only the bounded reassembly state survives until its protocol TTL so
        // an authenticated peer cannot erase an incomplete-message outcome by
        // closing its callback early.
        self.queues = InboundQueues::new();
        self.active = FuturesUnordered::new();
        self.active_lanes = [None; INBOUND_LANE_COUNT];
        self.reassembly_handoff_barrier = None;
        if !self.processor.prepare_reassembly_for_close().await {
            return;
        }
        loop {
            self.cleanup_expired_reassembly().await;
            if !self.processor.has_pending_reassembly().await {
                return;
            }
            if !try_sleep(self.reassembly_cleanup_delay()).await {
                tracing::error!(
                    "disabled close-time reassembly cleanup after timer scheduling failed"
                );
                self.processor
                    .discard_reassembly_after_close_timer_failure()
                    .await;
                return;
            }
        }
    }

    fn handle_input(&mut self, command: Option<InboundCommand>) {
        match command {
            Some(command) => self.handle_command(command),
            None => self.input_closed = true,
        }
    }

    fn handle_command(&mut self, command: InboundCommand) {
        match command {
            InboundCommand::Pending { sequence, lane } => self.queues.push_pending(sequence, lane),
            InboundCommand::Ready(event) => self.queues.push_ready(*event),
            InboundCommand::Cancel { sequence, lane } => self.queues.cancel(sequence, lane),
        }
    }

    fn handle_completion(&mut self, completion: InboundTaskCompletion) {
        let active = inbound_lane_entry!(&mut self.active_lanes, completion.lane);
        if active.take() != Some(completion.sequence) {
            tracing::error!(
                lane = ?completion.lane,
                sequence = completion.sequence,
                "inbound actor completed a non-active sequence"
            );
        }
        if let Some(next) = completion.next {
            if next.lane().is_logical_data() {
                debug_assert!(self.reassembly_handoff_barrier.is_none());
                self.reassembly_handoff_barrier =
                    Some(ReassemblyHandoffBarrier::new(next.sequence));
            }
            self.queues.push_ready(next);
        }
    }

    fn release_started_reassembly_handoff(&mut self) {
        if self
            .reassembly_handoff_barrier
            .as_ref()
            .is_some_and(ReassemblyHandoffBarrier::has_started)
        {
            self.reassembly_handoff_barrier = None;
        }
    }
}

async fn process_event(
    processor: InboundProcessor,
    event: InboundEvent,
    handoff_started: Option<Arc<AtomicBool>>,
) -> InboundTaskCompletion {
    if let Some(started) = handoff_started {
        started.store(true, Ordering::Release);
    }
    let lane = event.lane();
    let sequence = event.sequence;
    match processor.pending_connection_admits(event.peer).await {
        Ok(true) => {
            #[cfg(test)]
            processor
                .logical
                .transport
                .inbound_tap_for_test()
                .observe(&event.payload);
        }
        Ok(false) => {
            finish_completion(event.completion, Ok(()));
            return InboundTaskCompletion {
                lane,
                sequence,
                next: None,
            };
        }
        Err(error) => {
            finish_completion(event.completion, Err(error));
            return InboundTaskCompletion {
                lane,
                sequence,
                next: None,
            };
        }
    }
    if event.lane() == InboundLane::Reassembly {
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        crate::simulation::wait_reassembly_service(event.wire_bytes).await;
        let next = process_chunk_event(&processor, event).await;
        return InboundTaskCompletion {
            lane,
            sequence,
            next,
        };
    }

    let result =
        process_logical_message(&processor.logical, &event.payload, event.prepared_message).await;
    finish_completion(event.completion, result);
    InboundTaskCompletion {
        lane,
        sequence,
        next: None,
    }
}

/// Deliver a locally sourced payload: dispatch, then the application inbox, exactly as for a
/// verified frame from a connection, but with no connection to gate on and never waiting.
///
/// Room for application data is reserved before the transaction is admitted, so a payload this
/// pass cannot hand over is left unadmitted for a later pass instead of being admitted, handled
/// and lost to the replay window.
pub(super) async fn deliver_local(
    pipeline: &LogicalInbound,
    payload: &MessagePayload,
) -> Result<LocalDelivered> {
    let message = payload.transaction.data::<crate::message::Message>()?;
    let lane = InboundLane::from_kind(crate::message::MessageKind::from_message(&message));
    let application = pipeline.application_message(payload, message.clone());
    let room = match &application {
        Some(application) => match pipeline.reserve_room(application) {
            Some(room) => Some(room),
            None => return Ok(LocalDelivered::Deferred),
        },
        None => None,
    };
    #[cfg(test)]
    pipeline.transport.inbound_tap_for_test().observe(payload);
    // A local payload crossed no connection, so no neighbour can have its admission delegated.
    pipeline
        .admit_final_transaction(payload, &message, lane, EdgeRelation::Remote)
        .await?;
    pipeline.handle_payload(payload, Some(message)).await?;
    if let (Some(room), Some(application)) = (room, application) {
        room.fill(application);
    }
    Ok(LocalDelivered::Delivered)
}

/// What [`deliver_local`] did with a payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LocalDelivered {
    /// Admitted, handled, and its application data, if any, queued.
    Delivered,
    /// The application inbox had no room: nothing was admitted or handled.
    Deferred,
}

/// Dispatch `payload`, then queue its application data, if any, for the application, waiting
/// only for inbox room. Only the `Application` and `E2e` lanes carry application data, so only
/// they can wait here.
async fn process_logical_message(
    pipeline: &LogicalInbound,
    payload: &MessagePayload,
    prepared_message: Option<crate::message::Message>,
) -> Result<()> {
    let message = pipeline.handle_payload(payload, prepared_message).await?;
    if let Some(application) = pipeline.application_message(payload, message) {
        pipeline.deliver(application).await;
    }
    Ok(())
}

fn finish_completion(completion: InboundCompletion, result: Result<()>) {
    match completion {
        InboundCompletion::Await(reply) => {
            let _ = reply.send(result);
        }
        InboundCompletion::LogHeld { peer } => {
            if let Err(error) = result {
                tracing::warn!(
                    peer = %peer,
                    error = ?error,
                    "failed to deliver a message released from a hold"
                );
            }
        }
    }
}

#[cfg(test)]
mod test_inbound;
