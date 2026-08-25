//! Bounded inbound admission and per-lane actor scheduling.
//!
//! Raw transport frames are admitted synchronously when their lane ticket reaches the front.
//! Every valid frame fits its lane's fixed reservation; shared capacity may be borrowed,
//! but capacity pressure fails rather than parking ingress behind the actor that releases it.

use std::collections::BTreeMap;
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
use rings_transport::core::callback::InboundFrameClass;
use rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;
use web_time::Instant;

use super::CallbackError;
use super::InboundProcessor;
use super::PayloadHandlingError;
use super::PreparedInboundFrame;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::MessagePayload;
use crate::utils::sleep;
use crate::utils::ReservedCapacity;

mod lane;
mod reassembly;
mod ticket;

use self::lane::InboundLane;
use self::lane::INBOUND_LANE_COUNT;
use self::reassembly::process_chunk_event;
use self::ticket::InboundCommand;
use self::ticket::InboundSender;
use self::ticket::InboundTicket;

const INBOUND_MAILBOX_CAPACITY: usize = 256;
const INBOUND_MAILBOX_BYTE_CAPACITY: usize = 256 * 1024 * 1024;
const INBOUND_RESERVED_TRANSFERS_PER_LANE: usize = 16;
const INBOUND_RESERVED_BYTES_PER_LANE: usize = 1024 * 1024;
const INBOUND_RESERVED_TRANSFERS: [usize; INBOUND_LANE_COUNT] =
    [INBOUND_RESERVED_TRANSFERS_PER_LANE; INBOUND_LANE_COUNT];
const INBOUND_RESERVED_BYTES: [usize; INBOUND_LANE_COUNT] =
    [INBOUND_RESERVED_BYTES_PER_LANE; INBOUND_LANE_COUNT];
const INBOUND_PEER_CAPACITY: usize = 32;
const INBOUND_PEER_BYTE_CAPACITY: usize = 128 * 1024 * 1024;
const INBOUND_COMMAND_DRAIN_BUDGET: usize = 32;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
const REASSEMBLY_CLEANUP_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(not(all(test, feature = "dummy", not(target_family = "wasm"))))]
const REASSEMBLY_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
const _: () = {
    assert!(INBOUND_PEER_CAPACITY < INBOUND_MAILBOX_CAPACITY);
    assert!(INBOUND_PEER_BYTE_CAPACITY < INBOUND_MAILBOX_BYTE_CAPACITY);
    assert!(crate::consts::TRANSPORT_MAX_SIZE * 2 <= INBOUND_PEER_BYTE_CAPACITY);
    assert!(INBOUND_RESERVED_TRANSFERS_PER_LANE * INBOUND_LANE_COUNT <= INBOUND_MAILBOX_CAPACITY);
    assert!(INBOUND_RESERVED_BYTES_PER_LANE * INBOUND_LANE_COUNT <= INBOUND_MAILBOX_BYTE_CAPACITY);
    assert!(memory_reservation(MAX_DATA_CHANNEL_MESSAGE_SIZE) <= INBOUND_RESERVED_BYTES_PER_LANE);
};

#[derive(Clone, Copy)]
struct InboundCapacityState {
    messages: ReservedCapacity<INBOUND_LANE_COUNT>,
    bytes: ReservedCapacity<INBOUND_LANE_COUNT>,
}

impl InboundCapacityState {
    const fn new() -> Self {
        Self {
            messages: ReservedCapacity::new(),
            bytes: ReservedCapacity::new(),
        }
    }
    fn try_reserve(
        &mut self,
        lane: InboundLane,
        bytes: usize,
    ) -> std::result::Result<(), CapacityRejection> {
        if !self.messages.try_reserve(
            lane.index(),
            1,
            INBOUND_MAILBOX_CAPACITY,
            &INBOUND_RESERVED_TRANSFERS,
        ) {
            return Err(CapacityRejection::Count);
        }
        if !self.bytes.try_reserve(
            lane.index(),
            bytes,
            INBOUND_MAILBOX_BYTE_CAPACITY,
            &INBOUND_RESERVED_BYTES,
        ) {
            self.messages.release(lane.index(), 1);
            return Err(CapacityRejection::Bytes);
        }
        Ok(())
    }
    fn release(&mut self, lane: InboundLane, bytes: usize) {
        self.messages.release(lane.index(), 1);
        self.bytes.release(lane.index(), bytes);
    }
}

enum CapacityRejection {
    Count,
    Bytes,
}

const PEER_RESERVATION: [usize; 1] = [0];

#[derive(Clone, Copy)]
struct InboundPeerCapacityState {
    messages: ReservedCapacity<1>,
    bytes: ReservedCapacity<1>,
}

impl Default for InboundPeerCapacityState {
    fn default() -> Self {
        Self {
            messages: ReservedCapacity::new(),
            bytes: ReservedCapacity::new(),
        }
    }
}

impl InboundPeerCapacityState {
    fn try_reserve(&mut self, bytes: usize) -> std::result::Result<(), CapacityRejection> {
        if !self
            .messages
            .try_reserve(0, 1, INBOUND_PEER_CAPACITY, &PEER_RESERVATION)
        {
            return Err(CapacityRejection::Count);
        }
        if !self
            .bytes
            .try_reserve(0, bytes, INBOUND_PEER_BYTE_CAPACITY, &PEER_RESERVATION)
        {
            self.messages.release(0, 1);
            return Err(CapacityRejection::Bytes);
        }
        Ok(())
    }

    fn release(&mut self, bytes: usize) {
        self.messages.release(0, 1);
        self.bytes.release(0, bytes);
    }

    const fn is_idle(self) -> bool {
        self.messages.admitted() == 0
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
            Err(CapacityRejection::Count) => {
                return Err(Error::InboundPeerCapacityExceeded {
                    peer,
                    capacity: INBOUND_PEER_CAPACITY,
                });
            }
            Err(CapacityRejection::Bytes) => return Err(peer_memory_capacity_error(peer, bytes)),
        }
        match state.try_reserve(lane, bytes) {
            Ok(()) => {}
            Err(CapacityRejection::Count) => {
                return Err(Error::InboundMailboxCapacityExceeded {
                    capacity: INBOUND_MAILBOX_CAPACITY,
                });
            }
            Err(CapacityRejection::Bytes) => return Err(memory_capacity_error(bytes)),
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
            .messages
            .admitted()
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
            Err(CapacityRejection::Count) => {
                return Err(Error::InboundPeerCapacityExceeded {
                    peer: self.peer,
                    capacity: INBOUND_PEER_CAPACITY,
                });
            }
            Err(CapacityRejection::Bytes) => {
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
            Err(CapacityRejection::Count) => Err(Error::InboundMailboxCapacityExceeded {
                capacity: INBOUND_MAILBOX_CAPACITY,
            }),
            Err(CapacityRejection::Bytes) => Err(memory_capacity_error(bytes)),
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
}

type InboundReply = oneshot::Sender<std::result::Result<(), InboundFailure>>;

struct InboundEvent {
    sequence: u64,
    peer: Option<Did>,
    payload: MessagePayload,
    prepared_message: Option<crate::message::Message>,
    lane: InboundLane,
    is_chunk: bool,
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
        bytes: &[u8],
        prepared: PreparedInboundFrame,
    ) -> Result<()> {
        self.ensure_actor_available()?;
        let lane = InboundLane::from_frame_class(prepared.class);
        let is_chunk = prepared.class == InboundFrameClass::Reassembly;
        self.submit_to_lane(processor, peer, bytes, lane, is_chunk, Some(prepared))
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
        bytes: &[u8],
        lane: InboundLane,
        is_chunk: bool,
        prepared: Option<PreparedInboundFrame>,
    ) -> Result<()> {
        let mut ticket = self.reserve_ticket(lane)?;
        ticket.wait_for_admission_turn().await;
        let permit = self
            .capacity
            .acquire(peer, lane, memory_reservation(bytes.len()))?;
        ticket.release_admission_turn();
        if !processor.pending_connection_allows_message(peer).await? {
            return Ok(());
        }
        let decoded = decode_payload(processor, peer, bytes, prepared).await?;
        let (reply, completion) = oneshot::channel();
        let sequence = ticket.sequence();
        ticket.commit(InboundEvent {
            sequence,
            peer,
            payload: decoded.payload,
            prepared_message: decoded.prepared_message,
            lane,
            is_chunk,
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
        if let Some(queue) = self.lanes.get_mut(lane.index()) {
            debug_assert!(queue.back().is_none_or(|entry| entry.sequence < sequence));
            queue.push_back(InboundQueueEntry {
                sequence,
                event: None,
            });
        }
    }

    fn push_ready(&mut self, event: InboundEvent) {
        if let Some(queue) = self.lanes.get_mut(event.lane().index()) {
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
    }

    fn cancel(&mut self, sequence: u64, lane: InboundLane) {
        if let Some(queue) = self.lanes.get_mut(lane.index()) {
            if let Some(index) = queue.iter().position(|entry| entry.sequence == sequence) {
                queue.remove(index);
            }
        }
    }

    fn pop(&mut self, lane: InboundLane) -> Option<InboundEvent> {
        let queue = self.lanes.get_mut(lane.index())?;
        queue.front()?.event.as_ref()?;
        queue.pop_front()?.event
    }

    fn front_sequence(&self, lane: InboundLane) -> Option<u64> {
        self.lanes
            .get(lane.index())
            .and_then(VecDeque::front)
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
        for (index, active_lane) in self.active_lanes.iter_mut().enumerate() {
            if active_lane.is_some() {
                continue;
            }
            let Some(lane) = InboundLane::from_index(index) else {
                continue;
            };
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
            self.active_lanes
                .get(InboundLane::REASSEMBLY.index())
                .copied()
                .flatten(),
            self.queues.front_sequence(InboundLane::REASSEMBLY),
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
        if let Some(active) = self.active_lanes.get_mut(completion.lane.index()) {
            if active.take() != Some(completion.sequence) {
                tracing::error!(
                    lane = ?completion.lane,
                    sequence = completion.sequence,
                    "inbound actor completed a non-active sequence"
                );
            }
        } else {
            tracing::error!(lane = ?completion.lane, "inbound actor completed an unknown lane");
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
        lane == InboundLane::REASSEMBLY || (lane.is_logical_data() && sequence > self.sequence)
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
        Ok(true) => {}
        Ok(false) => {
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
    if event.is_chunk {
        let next = process_chunk_event(&processor, event).await;
        return InboundTaskCompletion {
            lane,
            sequence,
            next,
        };
    }

    let result = process_logical_message(&processor, &event.payload, event.prepared_message)
        .await
        .map_err(|error| match error {
            PayloadHandlingError::Core(error) => InboundFailure::Core(error),
            PayloadHandlingError::Callback(error) => InboundFailure::Callback(error),
        });
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
) -> std::result::Result<bool, InboundFailure> {
    if !processor
        .pending_connection_allows_message(event.peer)
        .await
        .map_err(InboundFailure::Core)?
    {
        return Ok(false);
    }
    processor
        .callback
        .on_validate(&event.payload)
        .await
        .map_err(InboundFailure::Validation)?;
    processor
        .pending_connection_allows_message(event.peer)
        .await
        .map_err(InboundFailure::Core)
}

async fn process_logical_message(
    processor: &InboundProcessor,
    payload: &MessagePayload,
    prepared_message: Option<crate::message::Message>,
) -> std::result::Result<(), PayloadHandlingError> {
    processor.handle_payload(payload, prepared_message).await
}

struct DecodedInboundFrame {
    payload: MessagePayload,
    prepared_message: Option<crate::message::Message>,
}

async fn decode_payload(
    processor: &InboundProcessor,
    peer: Option<Did>,
    bytes: &[u8],
    prepared: Option<PreparedInboundFrame>,
) -> Result<DecodedInboundFrame> {
    match prepared {
        Some(PreparedInboundFrame {
            payload, message, ..
        }) => {
            let payload = processor.accept_preverified_message(peer, payload).await?;
            Ok(DecodedInboundFrame {
                payload,
                prepared_message: Some(message),
            })
        }
        None => {
            let payload = processor.decode_verified_message(peer, bytes).await?;
            Ok(DecodedInboundFrame {
                payload,
                prepared_message: None,
            })
        }
    }
}

fn finish_reply(reply: InboundReply, result: std::result::Result<(), InboundFailure>) {
    let _ = reply.send(result);
}

fn inbound_failure_error(failure: InboundFailure) -> Error {
    match failure {
        InboundFailure::Core(error) => error,
        InboundFailure::Validation(source) => Error::InboundValidationFailed { source },
        InboundFailure::Callback(source) => Error::InboundCallbackFailed { source },
    }
}

const fn memory_reservation(bytes: usize) -> usize {
    bytes.saturating_mul(2)
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
    let reserved_for_other_lanes = INBOUND_RESERVED_BYTES
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != lane.index())
        .map(|(_, bytes)| *bytes)
        .sum::<usize>();
    let limit = INBOUND_MAILBOX_BYTE_CAPACITY.saturating_sub(reserved_for_other_lanes);
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
