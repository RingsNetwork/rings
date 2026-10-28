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
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::stream::FuturesUnordered;
use futures::FutureExt;
use futures::StreamExt;
use rings_transport::core::pool::ChannelLane;

use super::logical::Arrival;
use super::InboundFrameLease;
use super::InboundProcessor;
use super::LogicalInbound;
use super::PreparedInboundFrame;
use super::WeakLogical;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::measure::Authentication;
use crate::message::MessagePayload;
use crate::message::Principal;
use crate::swarm::application::Delivered;
use crate::swarm::detached::spawn_detached;
use crate::utils::try_sleep;
use crate::utils::GenerationWitness;
use crate::utils::Instant;

mod barrier;
mod capacity;
mod lane;
mod reassembly;
mod reassembly_clock;
mod ticket;

use self::barrier::UnfinishedReassembly;
use self::capacity::memory_reservation;
pub(super) use self::capacity::HeldCharge;
pub(crate) use self::capacity::InboundCapacity;
pub(super) use self::capacity::InboundCapacityPermit;
use self::capacity::InboundCharge;
pub(crate) use self::lane::InboundLane;
use self::lane::INBOUND_LANE_COUNT;
use self::reassembly::process_chunk_event;
pub(super) use self::reassembly_clock::ReassemblyClock;
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
const _: () =
    assert!((rings_transport::core::pool::DATA_CHANNEL_POOL_SIZE as usize) < INBOUND_PEER_CAPACITY);
const INBOUND_PEER_BYTE_CAPACITY: usize = 128 * 1024 * 1024;
/// Bounds command work before completed lane tasks are observed.
const INBOUND_COMMAND_DRAIN_BUDGET: usize = 32;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
const REASSEMBLY_CLEANUP_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(not(all(test, feature = "dummy", not(target_family = "wasm"))))]
const REASSEMBLY_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
/// Where the drainer that handed an event over learns its logical outcome.
type InboundReply = oneshot::Sender<Result<()>>;

/// How the submitter of an event learns its logical outcome, which is also how the transport
/// lane that carried it is paced.
///
/// ```text
///   lane FIFO ─drainer─▶ hand-off (credit returned) ─▶ actor ─▶ handler done ─▶ reply
///        ▲                                                                       │
///        └──────────── the drainer takes the lane's next frame only now ◀────────┘
/// ```
///
/// Law (lane-local pacing). An arrived frame is submitted with [`Self::Await`], and the
/// transport's drainer of its lane hands the next frame over only once this one completed, so
/// every transport lane of a peer has at most one event in the actor. A peer has
/// `DATA_CHANNEL_POOL_SIZE` lanes, fewer than the events of its per-peer share
/// (`INBOUND_PEER_CAPACITY`), so arrived frames never contend for those events, which the share
/// reserves for no lane; its bytes hold at most one data-channel message per lane, the peer's
/// own. A lane slow to process holds its own frames, and their transport credit, in its FIFO,
/// and backpressure reaches the peer through that lane's credit alone (per-lane isolation). Without
/// it, credit returned at hand-off would let one lane fill the shared per-peer capacity and
/// stall the others, lane 0 included; the composed liveness model refutes that design
/// (`HandOffWithoutAwait`). The dummy transport's "delivered = processed" rests on it too.
///
/// A frame released from the pre-admission hold is submitted with [`Self::LogHeld`] and is not
/// paced: the hold bounds how many are released, once, at admission.
enum InboundCompletion {
    /// Reply with the outcome once the event's logical processing is done.
    Await(InboundReply),
    /// Nobody awaits: a failure is logged against `peer`.
    LogHeld { peer: Did },
}

struct InboundEvent {
    sequence: u64,
    peer: Option<Did>,
    authentication: Authentication,
    payload: MessagePayload,
    prepared_message: Option<crate::message::Message>,
    lane: InboundLane,
    /// The transport lane the frame arrived on; a chunk's carrier is checked against its
    /// message once reassembled.
    carrier: Option<ChannelLane>,
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
pub(crate) struct InboundMailbox {
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
        processor: InboundProcessor<WeakLogical>,
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
        let actor_available = spawn_detached(Box::pin(actor.run())).is_ok();
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
                    carrier,
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
            let arrival = Arrival {
                peer,
                authentication,
            };
            processor
                .admit_final_transaction(&prepared.payload, &prepared.message, lane, arrival)
                .await?;
        }
        // Capacity before the ticket: an arrival waiting for capacity holds no sequence, so
        // the actor's ordering never waits on it.
        let introducer = Arrival {
            peer,
            authentication,
        }
        .introducer(processor.logical.transport.dht.edges());
        let charge = arrival_charge(&prepared.message, bytes.len(), (carrier, introducer));
        let permit = self.capacity.acquire((peer, lane, carrier), charge).await?;
        let ticket = self.reserve_ticket(lane, lane.category(carrier))?;
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
            carrier,
            wire_bytes,
            permit,
            completion,
        })?;
        Ok(())
    }

    /// Reserve the next sequence on `lane`, for an event ordered in `category`.
    fn reserve_ticket(&self, lane: InboundLane, category: InboundLane) -> Result<InboundTicket> {
        self.sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reserve(lane, category)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn admitted_count_for_test(&self) -> usize {
        self.capacity.admitted_count_for_test()
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn await_admitted_count_for_test(&self, predicate: impl Fn(usize) -> bool) {
        self.capacity.await_admitted_count_for_test(predicate).await;
    }

    /// Wait until the number of arrivals waiting for the mailbox satisfies `predicate`.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn await_waiting_for_test(&self, predicate: impl Fn(usize) -> bool) {
        self.capacity.await_waiting_for_test(predicate).await;
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn await_handoffs_for_test(&self, predicate: impl Fn(u64) -> bool) {
        self.handoffs.await_until(predicate).await;
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn cleanup_passes_for_test(&self) -> u64 {
        self.cleanup_passes.get()
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn await_cleanup_passes_for_test(&self, predicate: impl Fn(u64) -> bool) {
        self.cleanup_passes.await_until(predicate).await;
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn hold_application_admission_for_test(&self) -> Result<impl Drop> {
        self.reserve_ticket(InboundLane::Application, InboundLane::Application)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn hold_application_capacity_for_test(
        &self,
        peer: crate::dht::Did,
    ) -> Result<InboundCapacityPermit> {
        self.capacity
            .try_acquire(Some(peer), InboundLane::Application, 1)
            .map_err(|refusal| refusal.into_error(Some(peer), 1))
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn close_for_test(&self) {
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

/// The inbound actor of one connection.
///
/// It runs until its mailbox closes, which happens when the connection's callback is dropped,
/// so it is the connection's, and holds the connection's stage weakly (the release law of
/// [`WeakLogical`]): each event and each cleanup pass binds it, and a pass that finds the
/// transport gone stops the actor, since nothing it holds can be delivered any more.
struct InboundActor {
    processor: InboundProcessor<WeakLogical>,
    receiver: mpsc::UnboundedReceiver<InboundCommand>,
    queues: InboundQueues,
    active: FuturesUnordered<InboundTaskFuture>,
    active_lanes: [Option<u64>; INBOUND_LANE_COUNT],
    /// The reassembly events not yet completed, which order the later frames of their category.
    unfinished: UnfinishedReassembly,
    reassembly_clock: ReassemblyClock,
    next_reassembly_cleanup: Instant,
    input_closed: bool,
    cleanup_passes: Arc<GenerationWitness>,
}

impl InboundActor {
    fn new(
        processor: InboundProcessor<WeakLogical>,
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
            unfinished: UnfinishedReassembly::default(),
            reassembly_clock,
            next_reassembly_cleanup: Instant::now() + REASSEMBLY_CLEANUP_INTERVAL,
            input_closed: false,
            cleanup_passes,
        }
    }

    async fn run(mut self) {
        loop {
            let cleanup_due = self.reassembly_cleanup_delay().is_zero();
            if cleanup_due && !self.cleanup_expired_reassembly().await {
                return;
            }
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

    /// Start the front event of every idle lane that may run: a lane of a category waits only
    /// for earlier unfinished reassembly of that category (load-bounds paper, Def.
    /// "Per-category barrier", #927).
    fn dispatch_runnable(&mut self) {
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
            if lane != InboundLane::Reassembly && self.unfinished.blocks(lane, sequence) {
                #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
                if lane == InboundLane::from_class(crate::message::MessageCategory::DhtControl) {
                    crate::simulation::record_control_waited_for_reassembly();
                }
                continue;
            }
            let Some(event) = self.queues.pop(lane) else {
                continue;
            };
            *active_lane = Some(sequence);
            self.active
                .push(Box::pin(process_event(self.processor.clone(), event)));
        }
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
            return self.cleanup_expired_reassembly().await;
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

    /// Remove the reassemblies expired by now, charging their peers. Post: `false` iff the
    /// transport is gone, and nothing was removed; the actor then stops.
    async fn cleanup_expired_reassembly(&mut self) -> bool {
        let Some(processor) = self.processor.upgrade() else {
            return false;
        };
        let now_ms = self.reassembly_clock.now_ms();
        processor.remove_expired_reassembly_at(now_ms).await;
        self.next_reassembly_cleanup = Instant::now() + REASSEMBLY_CLEANUP_INTERVAL;
        self.cleanup_passes.bump();
        true
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
        self.unfinished.clear();
        if !self.processor.prepare_reassembly_for_close().await {
            return;
        }
        loop {
            if !self.cleanup_expired_reassembly().await
                || !self.processor.has_pending_reassembly().await
            {
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
            InboundCommand::Pending {
                sequence,
                lane,
                category,
            } => {
                if lane == InboundLane::Reassembly {
                    self.unfinished.begin(sequence, category);
                }
                self.queues.push_pending(sequence, lane);
            }
            InboundCommand::Ready(event) => self.queues.push_ready(*event),
            InboundCommand::Cancel { sequence, lane } => {
                if lane == InboundLane::Reassembly {
                    self.unfinished.end(sequence);
                }
                self.queues.cancel(sequence, lane);
            }
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
        // A reassembly event leaves the barrier in the step that hands its message, if any, to
        // its category's lane, at its own sequence: ahead of every later frame of the category.
        if completion.lane == InboundLane::Reassembly {
            self.unfinished.end(completion.sequence);
        }
        if let Some(next) = completion.next {
            self.queues.push_ready(next);
        }
    }
}

/// What an arrival of `wire_bytes` carrying `message`, on `carrier` from the slot
/// `introducer`, reserves on the mailbox ledger (load-bounds paper, Law 4, #932): a chunked
/// message is charged once, at its first chunk `[0, n]` of `d₀` bytes, for the retained `n·d₀`
/// its buffer and its completed copy take (never less than its own frame), on the lane of its
/// carrier's class and in the open-message table; a later chunk is covered by that charge; any
/// other frame is charged its own retained bytes.
///
/// ```text
/// Chunk [0, n], d₀ ──▶ Message(max(retained(n·d₀), retained(wire)), lane(carrier), introducer)
/// Chunk [i > 0, n] ──▶ Covered
/// otherwise ─────────▶ Bytes(retained(wire))
/// ```
fn arrival_charge(
    message: &crate::message::Message,
    wire_bytes: usize,
    (carrier, introducer): (Option<ChannelLane>, Principal),
) -> InboundCharge {
    let frame = memory_reservation(wire_bytes);
    match message {
        crate::message::Message::Chunk(chunk) => match chunk.chunk {
            [0, total] => InboundCharge::Message {
                bytes: memory_reservation(total.saturating_mul(chunk.data.len())).max(frame),
                lane: InboundLane::of_carrier(carrier),
                introducer,
            },
            _ => InboundCharge::Covered,
        },
        _ => InboundCharge::Bytes(frame),
    }
}

async fn process_event(
    processor: InboundProcessor<WeakLogical>,
    event: InboundEvent,
) -> InboundTaskCompletion {
    let lane = event.lane();
    let sequence = event.sequence;
    // The event binds the transport for its span; one whose transport is gone is dropped with
    // its completion, which tells a waiting submitter that the mailbox closed.
    let Some(processor) = processor.upgrade() else {
        return InboundTaskCompletion {
            lane,
            sequence,
            next: None,
        };
    };
    match processor.pending_connection_admits(event.peer).await {
        Ok(true) => processor.logical.tap_inbound(&event.payload),
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

    processor.logical.pass_tapped_lane(lane).await;
    let introducer = Arrival {
        peer: event.peer,
        authentication: event.authentication,
    }
    .introducer(processor.logical.transport.dht.edges());
    let result = process_logical_message(
        &processor.logical,
        &event.payload,
        introducer,
        event.prepared_message,
    )
    .await;
    finish_completion(event.completion, result);
    InboundTaskCompletion {
        lane,
        sequence,
        next: None,
    }
}

/// Dispatch `payload`, delivered by `introducer`, then hand its application data, if any, to the
/// application inbox:
/// queued or, when full, dropped, counted and logged by the inbox, and told as
/// [`SwarmEvent::MessageDropped`](crate::swarm::application::SwarmEvent::MessageDropped) when it
/// came from an admitted peer (#969). Never waits on the application.
async fn process_logical_message(
    pipeline: &LogicalInbound,
    payload: &MessagePayload,
    introducer: Principal,
    prepared_message: Option<crate::message::Message>,
) -> Result<()> {
    let message = pipeline
        .handle_payload(payload, introducer, prepared_message)
        .await?;
    if let Some(application) = pipeline.application_message(payload, message) {
        let from = application.from;
        if pipeline.transport.application().outlet.deliver(application) == Delivered::Dropped {
            if let Err(error) = pipeline.transport.announce_message_dropped(from) {
                tracing::warn!(peer = %from, ?error, "failed to announce a dropped message");
            }
        }
    }
    Ok(())
}

/// Report an event's logical `result` as its `completion` asks: to the awaiting drainer, or to
/// the log for a message released from the pre-admission hold, which nobody awaits.
///
/// A view refusal is the protocol's answer, not a failure (#1151): it is logged here once, at
/// debug, and reported as done, so neither the drainer's transport nor the hold logs it again.
fn finish_completion(completion: InboundCompletion, result: Result<()>) {
    let result = result.or_else(|error| {
        if error.is_view_refusal() {
            tracing::debug!(error = ?error, "refused a message this node's view does not assign it");
            Ok(())
        } else {
            Err(error)
        }
    });
    match completion {
        InboundCompletion::Await(reply) => {
            let _ = reply.send(result);
        }
        InboundCompletion::LogHeld { peer } => {
            if let Err(error) = result {
                tracing::warn!(
                    peer = %peer,
                    error = ?error,
                    "failed to deliver a message released from the pre-admission hold"
                );
            }
        }
    }
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
mod test_admission_saturation;
#[cfg(test)]
mod test_inbound;
#[cfg(test)]
mod test_open_messages;
