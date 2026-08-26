//! Bounded inbound admission and per-lane actor scheduling.
//!
//! Transport first bounds undecoded frames and transfers an opaque capacity lease.
//! Core then verifies the frame and atomically reserves per-peer plus class-aware
//! actor capacity before waiting for its lane ticket. The raw transport lease is
//! released after the ticket wait, when both capacity ownership and lane order are
//! established. Decode, validation, reassembly, and dispatch retain only the core
//! permit. Capacity pressure fails without peer penalty rather than parking ingress
//! behind the actor that releases it.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::stream::FuturesUnordered;
use futures::FutureExt;
use futures::StreamExt;
use rings_transport::core::callback::InboundFrameCapacityLease;
use rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;
use web_time::Instant;

use super::CallbackError;
use super::InboundProcessor;
use super::PreparedInboundFrame;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::fair_admission::admissible_capacity;
use crate::fair_admission::retained_wire_bytes;
use crate::fair_admission::CountedReservationRejection;
use crate::fair_admission::CountedReservedCapacity;
use crate::message::MessagePayload;
use crate::utils::sleep;

mod deadline;
mod lane;
mod reassembly;
mod ticket;

use self::deadline::await_inbound_deadline;
use self::deadline::InboundDeadline;
pub(crate) use self::lane::InboundLane;
use self::lane::INBOUND_LANE_COUNT;
use self::reassembly::process_chunk_event;
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
/// Maximum time an application-owned inbound callback may retain one lane and
/// its capacity permit. The deadline cancels the future; callback
/// implementations must therefore be cancellation-safe at every suspension.
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
const INBOUND_CALLBACK_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(not(all(test, feature = "dummy", not(target_family = "wasm"))))]
const INBOUND_CALLBACK_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
const REASSEMBLY_CLEANUP_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(not(all(test, feature = "dummy", not(target_family = "wasm"))))]
const REASSEMBLY_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
const _: () = {
    // One peer cannot consume the node budget, every lane retains a fixed
    // minimum, and one maximum legal frame always fits that minimum.
    assert!(INBOUND_PEER_CAPACITY < INBOUND_MAILBOX_CAPACITY);
    assert!(INBOUND_PEER_BYTE_CAPACITY < INBOUND_MAILBOX_BYTE_CAPACITY);
    assert!(retained_wire_bytes(crate::consts::TRANSPORT_MAX_SIZE) <= INBOUND_PEER_BYTE_CAPACITY);
    assert!(INBOUND_RESERVED_TRANSFERS_PER_LANE * INBOUND_LANE_COUNT <= INBOUND_MAILBOX_CAPACITY);
    assert!(INBOUND_RESERVED_BYTES_PER_LANE * INBOUND_LANE_COUNT <= INBOUND_MAILBOX_BYTE_CAPACITY);
    assert!(memory_reservation(MAX_DATA_CHANNEL_MESSAGE_SIZE) <= INBOUND_RESERVED_BYTES_PER_LANE);
};

#[derive(Clone, Copy)]
struct InboundCapacityState(CountedReservedCapacity<INBOUND_LANE_COUNT>);

impl InboundCapacityState {
    const fn new() -> Self {
        Self(CountedReservedCapacity::new())
    }
    fn try_reserve(
        &mut self,
        lane: InboundLane,
        bytes: usize,
    ) -> std::result::Result<(), CountedReservationRejection> {
        CountedReservedCapacity::try_reserve(
            &mut self.0,
            lane.index(),
            bytes,
            INBOUND_MAILBOX_CAPACITY,
            &INBOUND_RESERVED_TRANSFERS,
            INBOUND_MAILBOX_BYTE_CAPACITY,
            &INBOUND_RESERVED_BYTES,
        )
    }
    fn release(&mut self, lane: InboundLane, bytes: usize) {
        self.0.release(lane.index(), bytes);
    }
}

const PEER_RESERVATION: [usize; 1] = [0];

#[derive(Clone, Copy, Default)]
struct InboundPeerCapacityState(CountedReservedCapacity<1>);

impl InboundPeerCapacityState {
    fn try_reserve(
        &mut self,
        bytes: usize,
    ) -> std::result::Result<(), CountedReservationRejection> {
        CountedReservedCapacity::try_reserve(
            &mut self.0,
            0,
            bytes,
            INBOUND_PEER_CAPACITY,
            &PEER_RESERVATION,
            INBOUND_PEER_BYTE_CAPACITY,
            &PEER_RESERVATION,
        )
    }

    fn release(&mut self, bytes: usize) {
        self.0.release(0, bytes);
    }

    const fn is_idle(self) -> bool {
        self.0.admitted_count() == 0
    }
}

pub(crate) struct InboundCapacity {
    state: Mutex<InboundCapacityState>,
    peer_states: Mutex<BTreeMap<Option<Did>, InboundPeerCapacityState>>,
}

impl InboundCapacity {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(InboundCapacityState::new()),
            peer_states: Mutex::new(BTreeMap::new()),
        }
    }

    fn try_acquire(
        self: &Arc<Self>,
        peer: Option<Did>,
        lane: InboundLane,
        bytes: usize,
    ) -> Result<InboundCapacityPermit> {
        let mut peer_states = self
            .peer_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next_peer = peer_states.get(&peer).copied().unwrap_or_default();
        match next_peer.try_reserve(bytes) {
            Ok(()) => {}
            Err(CountedReservationRejection::Count) => {
                return Err(Error::InboundPeerCapacityExceeded {
                    peer,
                    capacity: INBOUND_PEER_CAPACITY,
                });
            }
            Err(CountedReservationRejection::Bytes) => {
                return Err(peer_memory_capacity_error(peer, bytes));
            }
        }
        match state.try_reserve(lane, bytes) {
            Ok(()) => {}
            Err(CountedReservationRejection::Count) => {
                return Err(Error::InboundMailboxCapacityExceeded {
                    capacity: INBOUND_MAILBOX_CAPACITY,
                });
            }
            Err(CountedReservationRejection::Bytes) => return Err(memory_capacity_error(bytes)),
        }
        peer_states.insert(peer, next_peer);
        Ok(InboundCapacityPermit {
            capacity: self.clone(),
            peer,
            lane,
            bytes,
        })
    }

    fn acquire(
        self: &Arc<Self>,
        peer: Option<Did>,
        lane: InboundLane,
        bytes: usize,
    ) -> Result<InboundCapacityPermit> {
        validate_memory_request(lane, bytes)?;
        validate_peer_memory_request(peer, bytes)?;
        self.try_acquire(peer, lane, bytes)
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn admitted_count_for_test(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
            .admitted_count()
    }
}

struct InboundCapacityPermit {
    capacity: Arc<InboundCapacity>,
    peer: Option<Did>,
    lane: InboundLane,
    bytes: usize,
}

impl InboundCapacityPermit {
    fn try_transition(&mut self, lane: InboundLane, bytes: usize) -> Result<()> {
        if lane == self.lane && bytes == self.bytes {
            return Ok(());
        }
        validate_memory_request(lane, bytes)?;
        validate_peer_memory_request(self.peer, bytes)?;
        let mut peer_states = self
            .capacity
            .peer_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self
            .capacity
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next_peer = peer_states.get(&self.peer).copied().unwrap_or_default();
        next_peer.release(self.bytes);
        match next_peer.try_reserve(bytes) {
            Ok(()) => {}
            Err(CountedReservationRejection::Count) => {
                return Err(Error::InboundPeerCapacityExceeded {
                    peer: self.peer,
                    capacity: INBOUND_PEER_CAPACITY,
                });
            }
            Err(CountedReservationRejection::Bytes) => {
                return Err(peer_memory_capacity_error(self.peer, bytes));
            }
        }
        let mut next = *state;
        next.release(self.lane, self.bytes);
        match next.try_reserve(lane, bytes) {
            Ok(()) => {
                peer_states.insert(self.peer, next_peer);
                *state = next;
                self.lane = lane;
                self.bytes = bytes;
                Ok(())
            }
            Err(CountedReservationRejection::Count) => Err(Error::InboundMailboxCapacityExceeded {
                capacity: INBOUND_MAILBOX_CAPACITY,
            }),
            Err(CountedReservationRejection::Bytes) => Err(memory_capacity_error(bytes)),
        }
    }
}

impl Drop for InboundCapacityPermit {
    fn drop(&mut self) {
        let mut peer_states = self
            .capacity
            .peer_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self
            .capacity
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.release(self.lane, self.bytes);
        if let Some(peer_state) = peer_states.get_mut(&self.peer) {
            peer_state.release(self.bytes);
            if peer_state.is_idle() {
                peer_states.remove(&self.peer);
            }
        }
    }
}

enum InboundFailure {
    Core(Error),
    Validation(CallbackError),
    Callback(CallbackError),
    ValidationTimeout { peer: Option<Did> },
    ProcessingTimeout { peer: Option<Did> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InboundValidation {
    Dispatch,
    AcknowledgeDrop,
}

type InboundReply = oneshot::Sender<std::result::Result<(), InboundFailure>>;

struct InboundEvent {
    sequence: u64,
    peer: Option<Did>,
    payload: MessagePayload,
    prepared_message: Option<crate::message::Message>,
    lane: InboundLane,
    permit: InboundCapacityPermit,
    reply: InboundReply,
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
}

impl InboundMailbox {
    pub(super) fn spawn(processor: InboundProcessor, capacity: Arc<InboundCapacity>) -> Self {
        let (sender, receiver) = mpsc::unbounded();
        let actor_available = spawn_actor(InboundActor::new(processor, receiver));
        Self {
            sender: Arc::new(Mutex::new(InboundSender::new(sender))),
            capacity,
            actor_available,
        }
    }

    pub(super) async fn submit_prepared(
        &self,
        processor: &InboundProcessor,
        peer: Option<Did>,
        bytes: Bytes,
        prepared: PreparedInboundFrame,
        transport_capacity: Option<InboundFrameCapacityLease>,
    ) -> Result<()> {
        self.ensure_actor_available()?;
        let lane = prepared.lane;
        self.submit_to_lane(processor, peer, bytes, lane, prepared, transport_capacity)
            .await
    }

    fn ensure_actor_available(&self) -> Result<()> {
        if self.actor_available {
            Ok(())
        } else {
            Err(Error::InboundMailboxRuntimeUnavailable)
        }
    }

    async fn submit_to_lane(
        &self,
        processor: &InboundProcessor,
        peer: Option<Did>,
        bytes: Bytes,
        lane: InboundLane,
        prepared: PreparedInboundFrame,
        transport_capacity: Option<InboundFrameCapacityLease>,
    ) -> Result<()> {
        let mut ticket = self.reserve_ticket(lane)?;
        let permit = self
            .capacity
            .acquire(peer, lane, memory_reservation(bytes.len()))?;
        ticket.wait_for_admission_turn().await;
        let PreparedInboundFrame {
            payload, message, ..
        } = prepared;
        // Core now owns both retained decoded representations. Release the raw
        // bytes and their transport lease together at this handoff boundary.
        drop((bytes, transport_capacity));
        ticket.release_admission_turn();
        if !processor.pending_connection_allows_message(peer).await? {
            return Ok(());
        }
        let payload = processor.accept_preverified_message(peer, payload).await?;
        let (reply, completion) = oneshot::channel();
        let sequence = ticket.sequence();
        ticket.commit(InboundEvent {
            sequence,
            peer,
            payload,
            prepared_message: Some(message),
            lane,
            permit,
            reply,
        })?;
        match completion.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(inbound_failure_error(error)),
            Err(_) => Err(Error::InboundMailboxClosed),
        }
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
    pub(super) fn hold_application_admission_for_test(&self) -> Result<impl Drop> {
        self.reserve_ticket(InboundLane::Application)
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
pub(super) const fn capacity_for_test() -> usize {
    INBOUND_MAILBOX_CAPACITY
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(super) const fn application_capacity_for_test() -> usize {
    INBOUND_MAILBOX_CAPACITY - INBOUND_RESERVED_TRANSFERS_PER_LANE * (INBOUND_LANE_COUNT - 1)
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(super) const fn peer_capacity_for_test() -> usize {
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
    next_reassembly_cleanup: Instant,
    input_closed: bool,
}

impl InboundActor {
    fn new(processor: InboundProcessor, receiver: mpsc::UnboundedReceiver<InboundCommand>) -> Self {
        Self {
            processor,
            receiver,
            queues: InboundQueues::new(),
            active: FuturesUnordered::new(),
            active_lanes: [None; INBOUND_LANE_COUNT],
            reassembly_handoff_barrier: None,
            next_reassembly_cleanup: Instant::now() + REASSEMBLY_CLEANUP_INTERVAL,
            input_closed: false,
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
                return;
            }
            self.dispatch_runnable();
            if self.active.is_empty() {
                self.wait_for_input().await;
                continue;
            }
            let cleanup_delay = self.reassembly_cleanup_delay();
            let input = self.receiver.next().fuse();
            let completed = self.active.next().fuse();
            let cleanup = sleep(cleanup_delay).fuse();
            futures::pin_mut!(input, completed, cleanup);
            futures::select! {
                event = input => self.handle_input(event),
                completion = completed => {
                    if let Some(completion) = completion {
                        self.handle_completion(completion);
                    }
                },
                _ = cleanup => self.cleanup_expired_reassembly().await,
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
                .is_some_and(|barrier| barrier.blocks(lane, sequence))
            {
                continue;
            }
            // The nested class is unknown until reassembly completes. Preserve data-plane order
            // across that handoff, while keeping control traffic independent for liveness.
            if lane.is_logical_data()
                && reassembly_barrier.is_some_and(|barrier| sequence > barrier)
            {
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

    async fn wait_for_input(&mut self) {
        let cleanup_delay = self.reassembly_cleanup_delay();
        let input = self.receiver.next().fuse();
        let cleanup = sleep(cleanup_delay).fuse();
        futures::pin_mut!(input, cleanup);
        futures::select! {
            event = input => self.handle_input(event),
            _ = cleanup => self.cleanup_expired_reassembly().await,
        }
    }

    fn reassembly_cleanup_delay(&self) -> Duration {
        self.next_reassembly_cleanup
            .saturating_duration_since(Instant::now())
    }

    async fn cleanup_expired_reassembly(&mut self) {
        self.processor.remove_expired_reassembly().await;
        self.next_reassembly_cleanup = Instant::now() + REASSEMBLY_CLEANUP_INTERVAL;
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

struct ReassemblyHandoffBarrier {
    sequence: u64,
    started: Arc<AtomicBool>,
}

impl ReassemblyHandoffBarrier {
    fn new(sequence: u64) -> Self {
        Self {
            sequence,
            started: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start_marker(&self) -> Arc<AtomicBool> {
        self.started.clone()
    }

    fn has_started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    fn blocks(&self, lane: InboundLane, sequence: u64) -> bool {
        lane == InboundLane::Reassembly || (lane.is_logical_data() && sequence > self.sequence)
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
    match validate_event(&processor, &event).await {
        Ok(InboundValidation::Dispatch) => {}
        Ok(InboundValidation::AcknowledgeDrop) => {
            finish_reply(event.reply, Ok(()));
            return InboundTaskCompletion {
                lane,
                sequence,
                next: None,
            };
        }
        Err(error) => {
            finish_reply(event.reply, Err(error));
            return InboundTaskCompletion {
                lane,
                sequence,
                next: None,
            };
        }
    }
    if event.lane() == InboundLane::Reassembly {
        let next = process_chunk_event(&processor, event).await;
        return InboundTaskCompletion {
            lane,
            sequence,
            next,
        };
    }

    let result = process_logical_message(
        &processor,
        event.peer,
        &event.payload,
        event.prepared_message,
    )
    .await;
    finish_reply(event.reply, result);
    InboundTaskCompletion {
        lane,
        sequence,
        next: None,
    }
}

async fn validate_event(
    processor: &InboundProcessor,
    event: &InboundEvent,
) -> std::result::Result<InboundValidation, InboundFailure> {
    if !processor
        .pending_connection_allows_message(event.peer)
        .await
        .map_err(InboundFailure::Core)?
    {
        return Ok(InboundValidation::AcknowledgeDrop);
    }
    match await_inbound_deadline(
        processor.callback.on_validate(&event.payload),
        INBOUND_CALLBACK_TIMEOUT,
    )
    .await
    {
        InboundDeadline::Completed(result) => result.map_err(InboundFailure::Validation)?,
        InboundDeadline::TimedOut => {
            return Err(InboundFailure::ValidationTimeout { peer: event.peer });
        }
    }
    let still_admitted = processor
        .pending_connection_allows_message(event.peer)
        .await
        .map_err(InboundFailure::Core)?;
    Ok(if still_admitted {
        InboundValidation::Dispatch
    } else {
        InboundValidation::AcknowledgeDrop
    })
}

async fn process_logical_message(
    processor: &InboundProcessor,
    peer: Option<Did>,
    payload: &MessagePayload,
    prepared_message: Option<crate::message::Message>,
) -> std::result::Result<(), InboundFailure> {
    processor
        .handle_payload(payload, prepared_message)
        .await
        .map_err(InboundFailure::Core)?;
    if !processor.is_local_destination(payload) {
        return Ok(());
    }
    match await_inbound_deadline(processor.on_inbound(payload), INBOUND_CALLBACK_TIMEOUT).await {
        InboundDeadline::Completed(result) => result.map_err(InboundFailure::Callback),
        InboundDeadline::TimedOut => Err(InboundFailure::ProcessingTimeout { peer }),
    }
}

async fn decode_payload(
    processor: &InboundProcessor,
    peer: Option<Did>,
    bytes: &[u8],
) -> Result<MessagePayload> {
    processor.decode_verified_message(peer, bytes).await
}

fn finish_reply(reply: InboundReply, result: std::result::Result<(), InboundFailure>) {
    let _ = reply.send(result);
}

fn inbound_failure_error(failure: InboundFailure) -> Error {
    match failure {
        InboundFailure::Core(error) => error,
        InboundFailure::Validation(source) => Error::InboundValidationFailed { source },
        InboundFailure::Callback(source) => Error::InboundCallbackFailed { source },
        InboundFailure::ValidationTimeout { peer } => Error::InboundValidationTimeout {
            peer,
            timeout_ms: INBOUND_CALLBACK_TIMEOUT.as_millis(),
        },
        InboundFailure::ProcessingTimeout { peer } => Error::InboundProcessingTimeout {
            peer,
            timeout_ms: INBOUND_CALLBACK_TIMEOUT.as_millis(),
        },
    }
}

const fn memory_reservation(bytes: usize) -> usize {
    retained_wire_bytes(bytes)
}

fn memory_capacity_error(requested_bytes: usize) -> Error {
    Error::InboundMailboxMemoryCapacityExceeded {
        requested_bytes,
        capacity_bytes: INBOUND_MAILBOX_BYTE_CAPACITY,
    }
}

fn peer_memory_capacity_error(peer: Option<Did>, requested_bytes: usize) -> Error {
    Error::InboundPeerMemoryCapacityExceeded {
        peer,
        requested_bytes,
        capacity_bytes: INBOUND_PEER_BYTE_CAPACITY,
    }
}

fn validate_peer_memory_request(peer: Option<Did>, requested_bytes: usize) -> Result<()> {
    if requested_bytes > INBOUND_PEER_BYTE_CAPACITY {
        return Err(peer_memory_capacity_error(peer, requested_bytes));
    }
    Ok(())
}

fn validate_memory_request(lane: InboundLane, requested_bytes: usize) -> Result<()> {
    let limit = admissible_capacity(
        INBOUND_MAILBOX_BYTE_CAPACITY,
        &INBOUND_RESERVED_BYTES,
        lane.index(),
    );
    if requested_bytes > limit {
        return Err(Error::InboundMailboxMemoryCapacityExceeded {
            requested_bytes,
            capacity_bytes: limit,
        });
    }
    Ok(())
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
fn spawn_actor(actor: InboundActor) -> bool {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return false;
    };
    runtime.spawn(actor.run());
    true
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
fn spawn_actor(actor: InboundActor) -> bool {
    wasm_bindgen_futures::spawn_local(actor.run());
    true
}

#[cfg(test)]
mod tests;
