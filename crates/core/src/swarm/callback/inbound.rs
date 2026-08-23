use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Poll;

use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::stream::FuturesUnordered;
use futures::FutureExt;
use futures::StreamExt;

use super::CallbackError;
use super::InboundProcessor;
use super::PayloadHandlingError;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::MessageClass;
use crate::message::MessageMeta;
use crate::message::MessagePayload;
#[cfg(all(test, not(target_family = "wasm")))]
use crate::swarm::transport::TestActivityPermit;
#[cfg(all(test, not(target_family = "wasm")))]
use crate::swarm::transport::TestActivityTracker;
use crate::utils::fair_reservation_fits;
use crate::utils::fixed_reservation_covers;
use crate::utils::FairAdmission;
use crate::utils::FairWaitQueue;

mod ticket;

use self::ticket::InboundCommand;
use self::ticket::InboundSender;
use self::ticket::InboundTicket;

const INBOUND_MAILBOX_CAPACITY: usize = 256;
const INBOUND_MAILBOX_BYTE_CAPACITY: usize = 128 * 1024 * 1024;
const INBOUND_RESERVED_TRANSFERS_PER_LANE: usize = 16;
const INBOUND_RESERVED_BYTES_PER_LANE: usize = 1024 * 1024;
const INBOUND_RESERVED_TRANSFERS: [usize; INBOUND_LANE_COUNT] =
    [INBOUND_RESERVED_TRANSFERS_PER_LANE; INBOUND_LANE_COUNT];
const INBOUND_RESERVED_BYTES: [usize; INBOUND_LANE_COUNT] =
    [INBOUND_RESERVED_BYTES_PER_LANE; INBOUND_LANE_COUNT];
const INBOUND_PEER_CAPACITY: usize = 64;
const INBOUND_PEER_BYTE_CAPACITY: usize = crate::consts::TRANSPORT_MAX_SIZE * 2;
const INBOUND_COMMAND_DRAIN_BUDGET: usize = 32;
const INBOUND_LANE_COUNT: usize = MessageClass::COUNT + 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InboundLane {
    DhtControl,
    Reassembly,
    Storage,
    E2e,
    Application,
}

impl InboundLane {
    const ORDER: [Self; INBOUND_LANE_COUNT] = [
        Self::DhtControl,
        Self::Reassembly,
        Self::Storage,
        Self::E2e,
        Self::Application,
    ];

    const fn from_meta(meta: MessageMeta) -> Self {
        if meta.kind().is_chunk() {
            return Self::Reassembly;
        }
        match meta.class() {
            MessageClass::DhtControl => Self::DhtControl,
            MessageClass::Storage => Self::Storage,
            MessageClass::E2e => Self::E2e,
            MessageClass::Application => Self::Application,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::DhtControl => 0,
            Self::Reassembly => 1,
            Self::Storage => 2,
            Self::E2e => 3,
            Self::Application => 4,
        }
    }
}

#[derive(Clone, Copy)]
struct InboundCapacityState {
    admitted: usize,
    admitted_bytes: usize,
    admitted_by_lane: [usize; INBOUND_LANE_COUNT],
    admitted_bytes_by_lane: [usize; INBOUND_LANE_COUNT],
}

impl InboundCapacityState {
    const fn new() -> Self {
        Self {
            admitted: 0,
            admitted_bytes: 0,
            admitted_by_lane: [0; INBOUND_LANE_COUNT],
            admitted_bytes_by_lane: [0; INBOUND_LANE_COUNT],
        }
    }

    fn reservation_covers(&self, lane: InboundLane, bytes: usize) -> bool {
        fixed_reservation_covers(
            &self.admitted_by_lane,
            lane.index(),
            1,
            &INBOUND_RESERVED_TRANSFERS,
        ) && fixed_reservation_covers(
            &self.admitted_bytes_by_lane,
            lane.index(),
            bytes,
            &INBOUND_RESERVED_BYTES,
        )
    }

    fn try_reserve(
        &mut self,
        lane: InboundLane,
        bytes: usize,
    ) -> std::result::Result<(), CapacityRejection> {
        if !fair_reservation_fits(
            &self.admitted_by_lane,
            self.admitted,
            lane.index(),
            1,
            INBOUND_MAILBOX_CAPACITY,
            &INBOUND_RESERVED_TRANSFERS,
        ) {
            return Err(CapacityRejection::Count);
        }
        if !fair_reservation_fits(
            &self.admitted_bytes_by_lane,
            self.admitted_bytes,
            lane.index(),
            bytes,
            INBOUND_MAILBOX_BYTE_CAPACITY,
            &INBOUND_RESERVED_BYTES,
        ) {
            return Err(CapacityRejection::Bytes);
        }
        self.admitted += 1;
        self.admitted_bytes += bytes;
        if let Some(lane_count) = self.admitted_by_lane.get_mut(lane.index()) {
            *lane_count += 1;
        }
        if let Some(lane_bytes) = self.admitted_bytes_by_lane.get_mut(lane.index()) {
            *lane_bytes += bytes;
        }
        Ok(())
    }

    fn release(&mut self, lane: InboundLane, bytes: usize) {
        self.admitted = self.admitted.saturating_sub(1);
        self.admitted_bytes = self.admitted_bytes.saturating_sub(bytes);
        if let Some(lane_count) = self.admitted_by_lane.get_mut(lane.index()) {
            *lane_count = lane_count.saturating_sub(1);
        }
        if let Some(lane_bytes) = self.admitted_bytes_by_lane.get_mut(lane.index()) {
            *lane_bytes = lane_bytes.saturating_sub(bytes);
        }
    }
}

enum CapacityRejection {
    Count,
    Bytes,
}

#[derive(Clone, Copy, Default)]
struct InboundPeerCapacityState {
    admitted: usize,
    admitted_bytes: usize,
}

impl InboundPeerCapacityState {
    fn try_reserve(&mut self, bytes: usize) -> std::result::Result<(), CapacityRejection> {
        if self.admitted >= INBOUND_PEER_CAPACITY {
            return Err(CapacityRejection::Count);
        }
        if self.admitted_bytes.saturating_add(bytes) > INBOUND_PEER_BYTE_CAPACITY {
            return Err(CapacityRejection::Bytes);
        }
        self.admitted += 1;
        self.admitted_bytes += bytes;
        Ok(())
    }

    fn release(&mut self, bytes: usize) {
        self.admitted = self.admitted.saturating_sub(1);
        self.admitted_bytes = self.admitted_bytes.saturating_sub(bytes);
    }
}

pub(crate) struct InboundCapacity {
    state: Mutex<InboundCapacityState>,
    peer_states: Mutex<BTreeMap<Option<Did>, InboundPeerCapacityState>>,
    waiters: Arc<FairWaitQueue>,
    #[cfg(all(test, not(target_family = "wasm")))]
    test_activity: Arc<TestActivityTracker>,
}

impl InboundCapacity {
    pub(crate) fn new() -> Self {
        Self::from_parts(
            #[cfg(all(test, not(target_family = "wasm")))]
            Arc::new(TestActivityTracker::new()),
        )
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn with_test_activity(test_activity: Arc<TestActivityTracker>) -> Self {
        Self::from_parts(test_activity)
    }

    fn from_parts(
        #[cfg(all(test, not(target_family = "wasm")))] test_activity: Arc<TestActivityTracker>,
    ) -> Self {
        Self {
            state: Mutex::new(InboundCapacityState::new()),
            peer_states: Mutex::new(BTreeMap::new()),
            waiters: Arc::new(FairWaitQueue::new()),
            #[cfg(all(test, not(target_family = "wasm")))]
            test_activity,
        }
    }

    fn try_acquire(
        self: &Arc<Self>,
        peer: Option<Did>,
        lane: InboundLane,
        bytes: usize,
    ) -> Result<InboundCapacityPermit> {
        self.try_acquire_inner(peer, lane, bytes, false)
    }

    fn try_acquire_reserved(
        self: &Arc<Self>,
        peer: Option<Did>,
        lane: InboundLane,
        bytes: usize,
    ) -> Result<InboundCapacityPermit> {
        self.try_acquire_inner(peer, lane, bytes, true)
    }

    fn try_acquire_inner(
        self: &Arc<Self>,
        peer: Option<Did>,
        lane: InboundLane,
        bytes: usize,
        reserved_only: bool,
    ) -> Result<InboundCapacityPermit> {
        #[cfg(all(test, not(target_family = "wasm")))]
        let test_activity = self.test_activity.begin();
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
        if reserved_only && !state.reservation_covers(lane, bytes) {
            return Err(memory_capacity_error(bytes));
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
            #[cfg(all(test, not(target_family = "wasm")))]
            _test_activity: test_activity,
        })
    }

    async fn acquire(
        self: &Arc<Self>,
        peer: Option<Did>,
        lane: InboundLane,
        bytes: usize,
    ) -> Result<InboundCapacityPermit> {
        validate_memory_request(lane, bytes)?;
        validate_peer_memory_request(peer, bytes)?;
        if let Ok(permit) = self.try_acquire_reserved(peer, lane, bytes) {
            return Ok(permit);
        }
        if bytes <= INBOUND_RESERVED_BYTES_PER_LANE {
            return self
                .waiters
                .try_admit_unqueued(memory_capacity_error(bytes), || {
                    self.try_acquire(peer, lane, bytes)
                });
        }
        match self
            .waiters
            .admit_or_wait(bytes, memory_capacity_error(bytes), || {
                self.try_acquire(peer, lane, bytes)
            })? {
            FairAdmission::Ready(permit) => Ok(permit),
            FairAdmission::Waiting(mut waiter) => {
                poll_fn(|context| {
                    match waiter.poll(context, || self.try_acquire(peer, lane, bytes).ok()) {
                        Poll::Ready(Some(permit)) => Poll::Ready(Ok(permit)),
                        Poll::Ready(None) => Poll::Ready(Err(Error::InboundMailboxClosed)),
                        Poll::Pending => Poll::Pending,
                    }
                })
                .await
            }
        }
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn admitted_count_for_test(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .admitted
    }
}

struct InboundCapacityPermit {
    capacity: Arc<InboundCapacity>,
    peer: Option<Did>,
    lane: InboundLane,
    bytes: usize,
    #[cfg(all(test, not(target_family = "wasm")))]
    _test_activity: TestActivityPermit,
}

impl InboundCapacityPermit {
    fn try_transition(&mut self, lane: InboundLane, bytes: usize) -> Result<()> {
        if lane == self.lane && bytes == self.bytes {
            return Ok(());
        }
        validate_memory_request(lane, bytes)?;
        validate_peer_memory_request(self.peer, bytes)?;
        self.try_transition_inner(lane, bytes, false)
    }

    fn try_transition_inner(
        &mut self,
        lane: InboundLane,
        bytes: usize,
        reserved_only: bool,
    ) -> Result<()> {
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
        if reserved_only && !next.reservation_covers(lane, bytes) {
            return Err(memory_capacity_error(bytes));
        }
        match next.try_reserve(lane, bytes) {
            Ok(()) => {
                peer_states.insert(self.peer, next_peer);
                *state = next;
                self.lane = lane;
                self.bytes = bytes;
                drop(state);
                drop(peer_states);
                self.capacity.waiters.wake_front();
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
            if peer_state.admitted == 0 {
                peer_states.remove(&self.peer);
            }
        }
        drop(state);
        drop(peer_states);
        self.capacity.waiters.wake_front();
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
    meta: MessageMeta,
    permit: InboundCapacityPermit,
    reply: InboundReply,
}

impl InboundEvent {
    const fn lane(&self) -> InboundLane {
        InboundLane::from_meta(self.meta)
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

    pub(super) async fn submit(
        &self,
        processor: &InboundProcessor,
        peer: Option<Did>,
        bytes: &[u8],
    ) -> Result<()> {
        if !self.actor_available {
            return Err(Error::InboundMailboxRuntimeUnavailable);
        }
        let meta = match MessagePayload::message_meta_from_wire(bytes) {
            Ok(meta) => meta,
            Err(error) => {
                processor.record_receive_failure(peer).await;
                return Err(error);
            }
        };
        let lane = InboundLane::from_meta(meta);
        let mut ticket = self.reserve_ticket(lane)?;
        ticket.wait_for_admission_turn().await;
        let permit = self
            .capacity
            .acquire(peer, lane, memory_reservation(bytes.len()))
            .await?;
        ticket.release_admission_turn();
        if !processor.pending_connection_allows_message(peer).await? {
            return Ok(());
        }
        let payload = decode_payload(processor, peer, bytes).await?;
        let (reply, completion) = oneshot::channel();
        let sequence = ticket.sequence();
        ticket.commit(InboundEvent {
            sequence,
            peer,
            payload,
            meta,
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
            queue.push_back(InboundQueueEntry {
                sequence,
                event: None,
            });
            queue.make_contiguous().sort_by_key(|entry| entry.sequence);
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
            queue.push_back(InboundQueueEntry {
                sequence: event.sequence,
                event: Some(event),
            });
            queue.make_contiguous().sort_by_key(|entry| entry.sequence);
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
            input_closed: false,
        }
    }

    async fn run(mut self) {
        loop {
            self.drain_available();
            if self.input_closed {
                return;
            }
            self.dispatch_runnable();
            if self.active.is_empty() {
                self.wait_for_input().await;
                continue;
            }
            let input = self.receiver.next().fuse();
            let completed = self.active.next().fuse();
            futures::pin_mut!(input, completed);
            futures::select! {
                event = input => self.handle_input(event),
                completion = completed => {
                    if let Some(completion) = completion {
                        self.handle_completion(completion);
                    }
                }
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
        let reassembly_barrier = self
            .active_lanes
            .get(InboundLane::Reassembly.index())
            .copied()
            .flatten()
            .into_iter()
            .chain(self.queues.front_sequence(InboundLane::Reassembly))
            .min();
        for (lane, active) in InboundLane::ORDER
            .into_iter()
            .zip(self.active_lanes.iter_mut())
        {
            if active.is_some() {
                continue;
            }
            let Some(sequence) = self.queues.front_sequence(lane) else {
                continue;
            };
            if lane != InboundLane::Reassembly
                && reassembly_barrier.is_some_and(|barrier| sequence > barrier)
            {
                continue;
            }
            let Some(event) = self.queues.pop(lane) else {
                continue;
            };
            *active = Some(sequence);
            self.active
                .push(Box::pin(process_event(self.processor.clone(), event)));
        }
    }

    async fn wait_for_input(&mut self) {
        let event = self.receiver.next().await;
        self.handle_input(event);
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
            self.queues.push_ready(next);
        }
    }
}

async fn process_event(processor: InboundProcessor, event: InboundEvent) -> InboundTaskCompletion {
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
    if event.meta.kind().is_chunk() {
        let next = process_chunk_event(&processor, event).await;
        return InboundTaskCompletion {
            lane,
            sequence,
            next,
        };
    }

    let result = process_logical_message(&processor, &event.payload)
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

async fn process_chunk_event(
    processor: &InboundProcessor,
    mut event: InboundEvent,
) -> Option<InboundEvent> {
    let bytes = match processor.handle_chunk(&event.payload).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            finish_reply(event.reply, Ok(()));
            return None;
        }
        Err(error) => {
            processor.record_receive_failure(event.peer).await;
            finish_reply(event.reply, Err(InboundFailure::Core(error)));
            return None;
        }
    };
    let meta = match MessagePayload::message_meta_from_wire(bytes.as_ref()) {
        Ok(meta) => meta,
        Err(error) => {
            processor.record_receive_failure(event.peer).await;
            finish_reply(event.reply, Err(InboundFailure::Core(error)));
            return None;
        }
    };
    if meta.kind().is_chunk() {
        finish_reply(
            event.reply,
            Err(InboundFailure::Core(Error::NestedChunkMessage)),
        );
        return None;
    }
    if let Err(error) = event.permit.try_transition(
        InboundLane::from_meta(meta),
        memory_reservation(bytes.as_ref().len()),
    ) {
        finish_reply(event.reply, Err(InboundFailure::Core(error)));
        return None;
    }
    match decode_payload(processor, event.peer, bytes.as_ref()).await {
        Ok(payload) => Some(InboundEvent {
            sequence: event.sequence,
            peer: event.peer,
            payload,
            meta,
            permit: event.permit,
            reply: event.reply,
        }),
        Err(error) => {
            finish_reply(event.reply, Err(InboundFailure::Core(error)));
            None
        }
    }
}

async fn process_logical_message(
    processor: &InboundProcessor,
    payload: &MessagePayload,
) -> std::result::Result<(), PayloadHandlingError> {
    processor.handle_payload(payload).await
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
