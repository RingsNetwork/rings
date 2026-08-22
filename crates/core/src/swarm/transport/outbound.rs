use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::MutexGuard;

use bytes::Bytes;
use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::future::FutureExt;
use futures::SinkExt;
use futures::StreamExt;
use rings_transport::delivery::DeliveryFuture;

use super::delivery::await_delivery_or_cancel;
use super::delivery::frame_chunk;
use super::delivery::record_cancel_measurement;
use super::delivery::record_measurement;
use super::delivery::send_data_with_timeout;
use super::delivery::ChunkSendPermit;
use super::delivery::ChunkSendProgress;
use super::delivery::SendCompletionOutcome;
use super::AdmittedConnection;
use crate::chunk::Chunk;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::measure::MeasureCounter;
use crate::measure::MeasureImpl;
use crate::message::Message;
use crate::session::SessionSk;

const OUTBOUND_TRANSFER_QUEUE_CAPACITY: usize = 256;
const OUTBOUND_CONTROL_BURST: usize = 4;
const OUTBOUND_COMMAND_DRAIN_BUDGET: usize = 32;

type TransferResultSender = oneshot::Sender<Result<SendCompletionOutcome>>;

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(super) type ChunkFrames = Box<dyn Iterator<Item = Chunk> + Send>;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(super) type ChunkFrames = Box<dyn Iterator<Item = Chunk>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OutboundCompletion {
    Detached,
    Tracked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) enum TransferClass {
    DhtControl,
    Storage,
    E2e,
    Application,
}

pub(super) struct OutboundMessageMeta {
    kind: &'static str,
    class: TransferClass,
}

impl OutboundMessageMeta {
    const fn new(kind: &'static str, class: TransferClass) -> Self {
        Self { kind, class }
    }

    pub(super) fn from_message(message: &Message) -> Self {
        match message {
            Message::ConnectNodeSend(_) => Self::new("ConnectNodeSend", TransferClass::DhtControl),
            Message::ConnectNodeReport(_) => {
                Self::new("ConnectNodeReport", TransferClass::DhtControl)
            }
            Message::FindSuccessorSend(_) => {
                Self::new("FindSuccessorSend", TransferClass::DhtControl)
            }
            Message::FindSuccessorReport(_) => {
                Self::new("FindSuccessorReport", TransferClass::DhtControl)
            }
            Message::NotifyPredecessorSend(_) => {
                Self::new("NotifyPredecessorSend", TransferClass::DhtControl)
            }
            Message::NotifyPredecessorReport(_) => {
                Self::new("NotifyPredecessorReport", TransferClass::DhtControl)
            }
            Message::PeerLivenessProbe(_) => {
                Self::new("PeerLivenessProbe", TransferClass::DhtControl)
            }
            Message::PeerLivenessReport(_) => {
                Self::new("PeerLivenessReport", TransferClass::DhtControl)
            }
            Message::QueryForTopoInfoSend(_) => {
                Self::new("QueryForTopoInfoSend", TransferClass::DhtControl)
            }
            Message::QueryForTopoInfoReport(_) => {
                Self::new("QueryForTopoInfoReport", TransferClass::DhtControl)
            }
            Message::SearchEntry(_) => Self::new("SearchEntry", TransferClass::Storage),
            Message::FoundEntry(_) => Self::new("FoundEntry", TransferClass::Storage),
            Message::OperateEntry(_) => Self::new("OperateEntry", TransferClass::Storage),
            Message::SyncEntriesWithSuccessor(_) => {
                Self::new("SyncEntriesWithSuccessor", TransferClass::Storage)
            }
            Message::SyncEntriesWithSuccessorReport(_) => {
                Self::new("SyncEntriesWithSuccessorReport", TransferClass::Storage)
            }
            Message::E2eHandshakeRequest(_) => Self::new("E2eHandshakeRequest", TransferClass::E2e),
            Message::E2eHandshakeResponse(_) => {
                Self::new("E2eHandshakeResponse", TransferClass::E2e)
            }
            Message::E2eStreamFrame(_) => Self::new("E2eStreamFrame", TransferClass::E2e),
            Message::CustomMessage(_) => Self::new("CustomMessage", TransferClass::Application),
            Message::Chunk(_) => Self::new("Chunk", TransferClass::Application),
        }
    }

    pub(super) const fn kind(&self) -> &'static str {
        self.kind
    }

    pub(super) const fn class(&self) -> TransferClass {
        self.class
    }
}

#[derive(Clone, Copy)]
enum LowerClass {
    Storage,
    E2e,
    Application,
}

impl LowerClass {
    const fn next(self) -> Self {
        match self {
            Self::Storage => Self::E2e,
            Self::E2e => Self::Application,
            Self::Application => Self::Storage,
        }
    }
}

pub(super) struct TransferQueues<T> {
    control: VecDeque<T>,
    storage: VecDeque<T>,
    e2e: VecDeque<T>,
    application: VecDeque<T>,
    lower_cursor: LowerClass,
    consecutive_control: usize,
}

impl<T> Default for TransferQueues<T> {
    fn default() -> Self {
        Self {
            control: VecDeque::new(),
            storage: VecDeque::new(),
            e2e: VecDeque::new(),
            application: VecDeque::new(),
            lower_cursor: LowerClass::Storage,
            consecutive_control: 0,
        }
    }
}

impl<T> TransferQueues<T> {
    pub(super) fn push(&mut self, class: TransferClass, item: T) {
        match class {
            TransferClass::DhtControl => self.control.push_back(item),
            TransferClass::Storage => self.storage.push_back(item),
            TransferClass::E2e => self.e2e.push_back(item),
            TransferClass::Application => self.application.push_back(item),
        }
    }

    fn has_control(&self) -> bool {
        !self.control.is_empty()
    }

    fn has_lower(&self) -> bool {
        !(self.storage.is_empty() && self.e2e.is_empty() && self.application.is_empty())
    }

    fn pop_lower_from(&mut self, class: LowerClass) -> Option<T> {
        match class {
            LowerClass::Storage => self.storage.pop_front(),
            LowerClass::E2e => self.e2e.pop_front(),
            LowerClass::Application => self.application.pop_front(),
        }
    }

    fn pop_lower(&mut self) -> Option<T> {
        let mut class = self.lower_cursor;
        for _ in 0..3 {
            let next = class.next();
            if let Some(item) = self.pop_lower_from(class) {
                self.lower_cursor = next;
                return Some(item);
            }
            class = next;
        }
        None
    }

    pub(super) fn pop(&mut self) -> Option<T> {
        if self.has_control()
            && (self.consecutive_control < OUTBOUND_CONTROL_BURST || !self.has_lower())
        {
            self.consecutive_control = self.consecutive_control.saturating_add(1);
            return self.control.pop_front();
        }

        if let Some(item) = self.pop_lower() {
            self.consecutive_control = 0;
            return Some(item);
        }

        if self.has_control() {
            self.consecutive_control = self.consecutive_control.saturating_add(1);
            return self.control.pop_front();
        }

        None
    }

    fn len(&self) -> usize {
        self.control
            .len()
            .saturating_add(self.storage.len())
            .saturating_add(self.e2e.len())
            .saturating_add(self.application.len())
    }
}

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
    measure: Option<MeasureImpl>,
    source: FrameSource,
    first_result: Option<TransferResultSender>,
    final_result: Option<TransferResultSender>,
    admitted_frames: usize,
}

pub(super) struct OutboundTransferRoute {
    class: TransferClass,
    did: Did,
    admitted: AdmittedConnection,
    permit: ChunkSendPermit,
    measure: Option<MeasureImpl>,
}

impl OutboundTransferRoute {
    pub(super) fn new(
        class: TransferClass,
        did: Did,
        admitted: AdmittedConnection,
        permit: ChunkSendPermit,
        measure: Option<MeasureImpl>,
    ) -> Self {
        Self {
            class,
            did,
            admitted,
            permit,
            measure,
        }
    }
}

impl OutboundTransfer {
    pub(super) fn whole(
        route: OutboundTransferRoute,
        data: Bytes,
        completion: OutboundCompletion,
    ) -> (Self, oneshot::Receiver<Result<SendCompletionOutcome>>) {
        Self::new(route, FrameSource::Whole(Some(data)), completion)
    }

    pub(super) fn chunked(
        route: OutboundTransferRoute,
        session_sk: SessionSk,
        chunks: ChunkFrames,
        completion: OutboundCompletion,
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
        )
    }

    fn new(
        route: OutboundTransferRoute,
        source: FrameSource,
        completion: OutboundCompletion,
    ) -> (Self, oneshot::Receiver<Result<SendCompletionOutcome>>) {
        let (sender, receiver) = oneshot::channel();
        let (first_result, final_result) = match completion {
            OutboundCompletion::Detached => (Some(sender), None),
            OutboundCompletion::Tracked => (None, Some(sender)),
        };
        (
            Self {
                class: route.class,
                did: route.did,
                admitted: route.admitted,
                permit: route.permit,
                measure: route.measure,
                source,
                first_result,
                final_result,
                admitted_frames: 0,
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
        self.admitted_frames == 0
    }

    fn mark_frame_admitted(&mut self) {
        self.admitted_frames = self.admitted_frames.saturating_add(1);
        if self.admitted_frames == 1 {
            self.resolve_first(Ok(SendCompletionOutcome::Succeeded));
        }
    }

    fn resolve_first(&mut self, result: Result<SendCompletionOutcome>) {
        if let Some(sender) = self.first_result.take() {
            let _ = sender.send(result);
        }
    }

    fn resolve_final(&mut self, result: Result<SendCompletionOutcome>) {
        if let Some(sender) = self.final_result.take() {
            let _ = sender.send(result);
        } else if let Some(sender) = self.first_result.take() {
            let _ = sender.send(result);
        }
    }
}

struct QueuedTransfer {
    id: u64,
    transfer: OutboundTransfer,
}

struct DeliveryEvent {
    id: u64,
    result: ChunkSendProgress<Result<()>>,
}

enum OutboundCommand {
    Enqueue(Box<OutboundTransfer>),
    Delivery(DeliveryEvent),
    Shutdown,
}

enum CommandPoll {
    Ready(OutboundCommand),
    Empty,
    Closed,
}

#[derive(Clone)]
pub(super) struct OutboundPeerHandle {
    sender: mpsc::Sender<OutboundCommand>,
}

impl OutboundPeerHandle {
    pub(super) async fn submit(&self, transfer: OutboundTransfer) -> Result<()> {
        let mut sender = self.sender.clone();
        sender
            .send(OutboundCommand::Enqueue(Box::new(transfer)))
            .await
            .map_err(|_| Error::ChannelSendMessageFailed)
    }

    fn shutdown(&self) {
        let mut sender = self.sender.clone();
        spawn_outbound_task(async move {
            let _ = sender.send(OutboundCommand::Shutdown).await;
        });
    }
}

#[derive(Default)]
pub(super) struct OutboundSchedulers {
    peers: Mutex<BTreeMap<Did, OutboundPeerHandle>>,
}

impl OutboundSchedulers {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn handle(&self, peer: Did) -> OutboundPeerHandle {
        let mut peers = self.lock_peers();
        if let Some(handle) = peers.get(&peer) {
            return handle.clone();
        }
        let (sender, receiver) = mpsc::channel(OUTBOUND_TRANSFER_QUEUE_CAPACITY);
        let handle = OutboundPeerHandle {
            sender: sender.clone(),
        };
        spawn_worker(OutboundWorker::new(sender, receiver));
        peers.insert(peer, handle.clone());
        handle
    }

    pub(super) fn shutdown(&self, peer: Did) {
        let handle = self.lock_peers().remove(&peer);
        if let Some(handle) = handle {
            handle.shutdown();
        }
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(super) fn peer_count_for_test(&self) -> usize {
        self.lock_peers().len()
    }

    fn lock_peers(&self) -> MutexGuard<'_, BTreeMap<Did, OutboundPeerHandle>> {
        self.peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct OutboundWorker {
    sender: mpsc::Sender<OutboundCommand>,
    receiver: mpsc::Receiver<OutboundCommand>,
    ready: TransferQueues<QueuedTransfer>,
    waiting: HashMap<u64, OutboundTransfer>,
    next_id: u64,
    input_closed: bool,
}

impl OutboundWorker {
    fn new(
        sender: mpsc::Sender<OutboundCommand>,
        receiver: mpsc::Receiver<OutboundCommand>,
    ) -> Self {
        Self {
            sender,
            receiver,
            ready: TransferQueues::default(),
            waiting: HashMap::new(),
            next_id: 0,
            input_closed: false,
        }
    }

    async fn run(mut self) {
        loop {
            if self.drain_available() {
                return;
            }
            if let Some(queued) = self.ready.pop() {
                self.admit_one_frame(queued).await;
                continue;
            }

            if self.input_closed && self.waiting.is_empty() {
                return;
            }

            match self.receiver.next().await {
                Some(command) => {
                    if self.handle_command(command) {
                        return;
                    }
                }
                None => {
                    self.input_closed = true;
                }
            }
        }
    }

    fn drain_available(&mut self) -> bool {
        for _ in 0..OUTBOUND_COMMAND_DRAIN_BUDGET {
            match self.poll_command() {
                CommandPoll::Ready(command) => {
                    if self.handle_command(command) {
                        return true;
                    }
                }
                CommandPoll::Closed => {
                    self.input_closed = true;
                    return false;
                }
                CommandPoll::Empty => return false,
            }
        }
        false
    }

    fn poll_command(&mut self) -> CommandPoll {
        match self.receiver.next().now_or_never() {
            Some(Some(command)) => CommandPoll::Ready(command),
            Some(None) => CommandPoll::Closed,
            None => CommandPoll::Empty,
        }
    }

    fn handle_command(&mut self, command: OutboundCommand) -> bool {
        match command {
            OutboundCommand::Enqueue(transfer) => self.enqueue_transfer(*transfer),
            OutboundCommand::Delivery(event) => self.handle_delivery(event),
            OutboundCommand::Shutdown => {
                self.shutdown();
                return true;
            }
        }
        false
    }

    fn active_transfer_count(&self) -> usize {
        self.ready.len().saturating_add(self.waiting.len())
    }

    fn enqueue_transfer(&mut self, mut transfer: OutboundTransfer) {
        if self.active_transfer_count() >= OUTBOUND_TRANSFER_QUEUE_CAPACITY {
            transfer.resolve_final(Err(Error::ChannelSendMessageFailed));
            return;
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let class = transfer.class();
        self.ready.push(class, QueuedTransfer { id, transfer });
    }

    fn handle_delivery(&mut self, event: DeliveryEvent) {
        let Some(mut transfer) = self.waiting.remove(&event.id) else {
            return;
        };
        match event.result {
            ChunkSendProgress::Ready(Ok(())) => {
                let class = transfer.class();
                self.ready.push(class, QueuedTransfer {
                    id: event.id,
                    transfer,
                });
            }
            ChunkSendProgress::Ready(Err(error)) => {
                let measure = transfer.measure.clone();
                let did = transfer.did;
                spawn_measurement(measure, did, MeasureCounter::FailedToSend);
                transfer.resolve_final(Err(error));
            }
            ChunkSendProgress::Cancelled(reason) => {
                let measure = transfer.measure.clone();
                let did = transfer.did;
                spawn_cancel_measurement(measure, did, reason);
                transfer.resolve_final(Ok(SendCompletionOutcome::Cancelled));
            }
        }
    }

    fn cancel_all(&mut self) {
        while let Some(mut queued) = self.ready.pop() {
            queued
                .transfer
                .resolve_final(Ok(SendCompletionOutcome::Cancelled));
        }
        for (_, mut transfer) in self.waiting.drain() {
            transfer.resolve_final(Ok(SendCompletionOutcome::Cancelled));
        }
    }

    fn shutdown(&mut self) {
        self.receiver.close();
        self.cancel_all();
        self.cancel_buffered_commands();
    }

    fn cancel_buffered_commands(&mut self) {
        loop {
            match self.poll_command() {
                CommandPoll::Ready(OutboundCommand::Enqueue(transfer)) => {
                    let mut transfer = *transfer;
                    transfer.resolve_final(Ok(SendCompletionOutcome::Cancelled));
                }
                CommandPoll::Ready(OutboundCommand::Delivery(_) | OutboundCommand::Shutdown) => {}
                CommandPoll::Closed => {
                    self.input_closed = true;
                    return;
                }
                CommandPoll::Empty => return,
            }
        }
    }

    async fn admit_one_frame(&mut self, mut queued: QueuedTransfer) {
        let before_first_frame = queued.transfer.is_before_first_frame();
        let frame = match queued.transfer.next_frame() {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                record_measurement(
                    queued.transfer.measure.clone(),
                    queued.transfer.did,
                    MeasureCounter::Sent,
                )
                .await;
                queued
                    .transfer
                    .resolve_final(Ok(SendCompletionOutcome::Succeeded));
                return;
            }
            Err(error) => {
                record_measurement(
                    queued.transfer.measure.clone(),
                    queued.transfer.did,
                    MeasureCounter::FailedToSend,
                )
                .await;
                queued.transfer.resolve_final(Err(error));
                return;
            }
        };
        let (bytes, context) = frame;
        let admission = send_data_with_timeout(
            &queued.transfer.admitted,
            bytes,
            &queued.transfer.permit,
            queued.transfer.did,
            context,
        )
        .await;

        match admission {
            ChunkSendProgress::Ready(Ok(delivery)) => {
                queued.transfer.mark_frame_admitted();
                self.spawn_delivery_wait(queued.id, delivery, &queued.transfer);
                self.waiting.insert(queued.id, queued.transfer);
            }
            ChunkSendProgress::Ready(Err(error)) => {
                if error.records_peer_send_failure() {
                    record_measurement(
                        queued.transfer.measure.clone(),
                        queued.transfer.did,
                        MeasureCounter::FailedToSend,
                    )
                    .await;
                }
                queued.transfer.resolve_final(Err(error));
            }
            ChunkSendProgress::Cancelled(reason) => {
                record_cancel_measurement(
                    queued.transfer.measure.clone(),
                    queued.transfer.did,
                    &reason,
                )
                .await;
                if before_first_frame {
                    match reason.resolve_initial() {
                        Ok(()) => queued
                            .transfer
                            .resolve_final(Ok(SendCompletionOutcome::Cancelled)),
                        Err(error) => queued.transfer.resolve_final(Err(error)),
                    }
                } else {
                    queued
                        .transfer
                        .resolve_final(Ok(SendCompletionOutcome::Cancelled));
                }
            }
        }
    }

    fn spawn_delivery_wait(&self, id: u64, delivery: DeliveryFuture, transfer: &OutboundTransfer) {
        let mut sender = self.sender.clone();
        let admitted = transfer.admitted.clone();
        let permit = transfer.permit.clone();
        let did = transfer.did;
        spawn_outbound_task(async move {
            let result =
                await_delivery_or_cancel(delivery, &admitted, &permit, did, "frame_delivery").await;
            let _ = sender
                .send(OutboundCommand::Delivery(DeliveryEvent { id, result }))
                .await;
        });
    }
}

fn spawn_measurement(measure: Option<MeasureImpl>, did: Did, counter: MeasureCounter) {
    spawn_outbound_task(async move {
        record_measurement(measure, did, counter).await;
    });
}

fn spawn_cancel_measurement(
    measure: Option<MeasureImpl>,
    did: Did,
    reason: super::delivery::ChunkSendCancelReason,
) {
    spawn_outbound_task(async move {
        record_cancel_measurement(measure, did, &reason).await;
    });
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
fn spawn_worker(worker: OutboundWorker) {
    wasm_bindgen_futures::spawn_local(worker.run());
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
fn spawn_worker(worker: OutboundWorker) {
    tokio::spawn(worker.run());
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
fn spawn_outbound_task(future: impl futures::Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(future);
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
fn spawn_outbound_task(future: impl futures::Future<Output = ()> + Send + 'static) {
    tokio::spawn(future);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pop_order(mut queues: TransferQueues<&'static str>, n: usize) -> Vec<&'static str> {
        let mut order = Vec::new();
        for _ in 0..n {
            let Some(item) = queues.pop() else {
                break;
            };
            order.push(item);
        }
        order
    }

    #[test]
    fn dht_control_preempts_queued_bulk_work() {
        let mut queues = TransferQueues::default();
        queues.push(TransferClass::Application, "app-1");
        queues.push(TransferClass::Application, "app-2");
        queues.push(TransferClass::DhtControl, "dht");

        assert_eq!(queues.pop(), Some("dht"));
    }

    #[test]
    fn continuous_dht_control_yields_to_lower_classes() {
        let mut queues = TransferQueues::default();
        for item in ["dht-1", "dht-2", "dht-3", "dht-4", "dht-5"] {
            queues.push(TransferClass::DhtControl, item);
        }
        queues.push(TransferClass::Application, "app");

        assert_eq!(pop_order(queues, 6), vec![
            "dht-1", "dht-2", "dht-3", "dht-4", "app", "dht-5"
        ]);
    }

    #[test]
    fn lower_classes_progress_round_robin() {
        let mut queues = TransferQueues::default();
        queues.push(TransferClass::Storage, "storage-1");
        queues.push(TransferClass::Storage, "storage-2");
        queues.push(TransferClass::E2e, "e2e");
        queues.push(TransferClass::Application, "app");

        assert_eq!(pop_order(queues, 4), vec![
            "storage-1",
            "e2e",
            "app",
            "storage-2"
        ]);
    }

    #[test]
    fn input_drain_yields_after_budget() {
        let (mut sender, receiver) = mpsc::channel(OUTBOUND_TRANSFER_QUEUE_CAPACITY);
        for id in 0..OUTBOUND_COMMAND_DRAIN_BUDGET.saturating_add(1) {
            assert!(sender
                .try_send(OutboundCommand::Delivery(DeliveryEvent {
                    id: id as u64,
                    result: ChunkSendProgress::Ready(Ok(())),
                }))
                .is_ok());
        }
        let mut worker = OutboundWorker::new(sender, receiver);

        assert!(!worker.drain_available());

        let mut remaining = 0;
        while matches!(worker.poll_command(), CommandPoll::Ready(_)) {
            remaining += 1;
        }
        assert_eq!(remaining, 1);
    }

    #[test]
    fn input_drain_leaves_open_empty_channel_unclosed() {
        let (sender, receiver) = mpsc::channel(OUTBOUND_TRANSFER_QUEUE_CAPACITY);
        let mut worker = OutboundWorker::new(sender, receiver);

        assert!(!worker.drain_available());
        assert!(!worker.input_closed);
    }

    #[test]
    fn poll_command_receives_message_after_empty_probe() {
        let (sender, receiver) = mpsc::channel(OUTBOUND_TRANSFER_QUEUE_CAPACITY);
        let mut external_sender = sender.clone();
        let mut worker = OutboundWorker::new(sender, receiver);

        assert!(matches!(worker.poll_command(), CommandPoll::Empty));
        assert!(external_sender
            .try_send(OutboundCommand::Delivery(DeliveryEvent {
                id: 7,
                result: ChunkSendProgress::Ready(Ok(())),
            }))
            .is_ok());

        assert!(matches!(
            worker.poll_command(),
            CommandPoll::Ready(OutboundCommand::Delivery(DeliveryEvent { id: 7, .. }))
        ));
    }

    #[test]
    fn shutdown_drains_buffered_commands_and_marks_input_closed() {
        let (mut sender, receiver) = mpsc::channel(OUTBOUND_TRANSFER_QUEUE_CAPACITY);
        assert!(sender
            .try_send(OutboundCommand::Delivery(DeliveryEvent {
                id: 7,
                result: ChunkSendProgress::Ready(Ok(())),
            }))
            .is_ok());
        let mut worker = OutboundWorker::new(sender, receiver);

        worker.shutdown();

        assert!(worker.input_closed);
        assert!(matches!(worker.poll_command(), CommandPoll::Closed));
    }

    #[test]
    fn message_classification_is_local_and_control_first() {
        let dht = Message::PeerLivenessProbe(crate::message::PeerLivenessProbe { sent_at_ms: 1 });
        let storage = Message::SyncEntriesWithSuccessor(crate::message::SyncEntriesWithSuccessor {
            purpose: crate::dht::StorageSyncPurpose::AdditiveRepair,
            destination: crate::dht::StorageSyncDestination::PhysicalOwner(Did::from(1_u32)),
            data: Vec::new(),
        });
        let app = Message::custom(b"hello");
        let dht_metadata = OutboundMessageMeta::from_message(&dht);
        let storage_metadata = OutboundMessageMeta::from_message(&storage);

        assert_eq!(dht_metadata.kind(), "PeerLivenessProbe");
        assert_eq!(dht_metadata.class(), TransferClass::DhtControl);
        assert_eq!(storage_metadata.kind(), "SyncEntriesWithSuccessor");
        assert_eq!(storage_metadata.class(), TransferClass::Storage);
        assert!(matches!(
            app,
            Ok(ref message) if {
                let metadata = OutboundMessageMeta::from_message(message);
                metadata.kind() == "CustomMessage"
                    && metadata.class() == TransferClass::Application
            }
        ));
    }
}
