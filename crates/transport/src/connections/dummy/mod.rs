use std::cell::Cell;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use lazy_static::lazy_static;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::callback::AdmittedInboundFrame;
use crate::callback::InboundFrameCapacity;
use crate::callback::InnerTransportCallback;
use crate::connection_ref::ConnectionRef;
use crate::core::callback::BoxedTransportCallback;
use crate::core::transport::stored_max_message_size;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateSnapshot;
use crate::core::transport::IrrevocableSendGuard;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportInterface;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::delivery::DeliveryFuture;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::parse_ice_servers_or_warn;
use crate::notifier::Notifier;
use crate::pool::Pool;
use crate::sync_utils::lock_recover;
use crate::webrtc_config::WebrtcUdpPortRange;

mod delay;
mod retirement;

use self::delay::random;
use self::delay::random_delay;
use self::retirement::DummyRetirementFence;

/// Max delay in ms on sending message
const DUMMY_DELAY_MAX: u64 = 100;
/// Min delay in ms on sending message
const DUMMY_DELAY_MIN: u64 = 10;
/// Config random delay when send message
const SEND_MESSAGE_DELAY: bool = true;

lazy_static! {
    static ref CONNS: DashMap<String, Arc<DummyConnection>> = DashMap::new();
}

struct DeliveryGate {
    waiting: AtomicBool,
    notify: Notify,
}

impl DeliveryGate {
    fn new() -> Self {
        Self {
            waiting: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }
}

thread_local! {
    /// Per-(test-)thread controlled-delivery state. THREAD-LOCAL on purpose: the
    /// flag and queue are scoped to the current thread so a controlled test is
    /// isolated from any other dummy test running in parallel. With the default
    /// current-thread `#[tokio::test]` runtime, all of a test's dummy activity
    /// (its connections' event listeners, sends, and the cascaded handlers) runs
    /// on that one thread — so only that test ever sees `CONTROLLED == true`, and
    /// only its events land in its own `DELIVERY` queue. Other tests, on other
    /// threads, keep auto-dispatching as usual.
    static CONTROLLED: Cell<bool> = const { Cell::new(false) };
    /// Test-only controlled delivery state, populated instead of auto-dispatching
    /// while `CONTROLLED` is on.
    static DELIVERY: RefCell<ControlledDeliveryState> = const { RefCell::new(ControlledDeliveryState::new()) };
    /// Test-only per-thread counter of data-channel messages dispatched by `send_message`, so a
    /// test can prove an expected send happened (or, after an error, did *not* happen). Thread-local
    /// for the same isolation reason as the controlled queue.
    static SENT_COUNT: Cell<usize> = const { Cell::new(0) };
    /// Test-only per-thread override for the negotiated `max_message_size` the dummy backend
    /// reports. `0` = report the default; a smaller value lets a test force the chunked send path
    /// through `do_send_payload` and exercise real reassembly. Thread-local for the same isolation
    /// reason as the controlled queue.
    static MAX_MESSAGE_SIZE: Cell<usize> = const { Cell::new(0) };
    /// Test-only per-thread override for the next lifecycle callback's cid. This lets dummy tests
    /// exercise malformed transport events without changing the production callback path.
    static NEXT_CALLBACK_CID: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Test-only per-thread switch that makes `webrtc_wait_for_data_channel_open` stay pending.
    /// This models a lifecycle notifier/callback wedge after a connection was already admitted.
    static WAIT_FOR_DATA_CHANNEL_OPEN_PENDING: Cell<bool> = const { Cell::new(false) };
    /// Test-only per-thread switch that makes `send_message` stay pending after
    /// the data channel is already open.
    static SEND_MESSAGE_PENDING: Cell<bool> = const { Cell::new(false) };
    /// Test-only releasable gate immediately before dummy message dispatch.
    static SEND_MESSAGE_GATE: RefCell<Option<Arc<Notify>>> = const { RefCell::new(None) };
    /// Whether a dummy send is currently suspended at [`SEND_MESSAGE_GATE`].
    static SEND_MESSAGE_GATE_WAITING: Cell<bool> = const { Cell::new(false) };
    /// Test-only releasable gate immediately after the send permit linearizes.
    static POST_PERMIT_SEND_GATE: RefCell<Option<Arc<Notify>>> = const { RefCell::new(None) };
    /// Whether a dummy send is suspended after its initial permit check.
    static POST_PERMIT_SEND_GATE_WAITING: Cell<bool> = const { Cell::new(false) };
    /// Test-only releasable gate after the send becomes irrevocable.
    static IRREVOCABLE_SEND_GATE: RefCell<Option<Arc<Notify>>> = const { RefCell::new(None) };
    /// Whether a dummy send is suspended after becoming irrevocable.
    static IRREVOCABLE_SEND_GATE_WAITING: Cell<bool> = const { Cell::new(false) };
    /// Test-only per-thread threshold that makes `send_message` stay pending after
    /// this many messages have already been dispatched.
    static SEND_MESSAGE_PENDING_AFTER_SENT_COUNT: Cell<Option<usize>> = const { Cell::new(None) };
    /// Test-only per-thread switch that returns a delivery future which never
    /// observes completion after the message has been accepted.
    static DELIVERY_FUTURE_PENDING: Cell<bool> = const { Cell::new(false) };
    /// Test-only per-thread switch that makes connection cleanup stay pending.
    static CLOSE_PENDING: Cell<bool> = const { Cell::new(false) };
    /// One-shot gate captured by the next accepted delivery future.
    static NEXT_DELIVERY_GATE: RefCell<Option<Arc<DeliveryGate>>> = const { RefCell::new(None) };
    /// Gate currently held by an accepted delivery future.
    static ACTIVE_DELIVERY_GATE: RefCell<Option<Arc<DeliveryGate>>> = const { RefCell::new(None) };
    /// Test-only per-thread switch that makes `send_message` report local success
    /// without dispatching the message to the remote callback.
    static DROP_MESSAGES: Cell<bool> = const { Cell::new(false) };
}

struct ControlledDeliveryState {
    queue: VecDeque<(String, Event)>,
    generation: u64,
}

impl ControlledDeliveryState {
    const fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            generation: 0,
        }
    }

    fn push_back(&mut self, entry: (String, Event)) {
        self.queue.push_back(entry);
        self.advance_generation();
    }

    fn remove(&mut self, index: usize) -> Option<(String, Event)> {
        let entry = self.queue.remove(index);
        if entry.is_some() {
            self.advance_generation();
        }
        entry
    }

    fn clear(&mut self) {
        if !self.queue.is_empty() {
            self.queue.clear();
            self.advance_generation();
        }
    }

    fn snapshot(&self) -> controlled::DeliverySnapshot {
        controlled::DeliverySnapshot::new(self.queue.len(), self.generation)
    }

    fn advance_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

#[cfg(test)]
mod test_dummy;

/// Test-only controlled delivery scheduler. When enabled (per thread), dummy
/// message/event delivery is queued instead of auto-dispatched, so a test can
/// drive the exact ordering and deterministically explore the timing-state space
/// (see `rings_core`'s `tests::default::test_dht_schedule`). Off by default; no effect
/// on normal runs.
pub mod controlled {
    use std::sync::Arc;

    use super::ACTIVE_DELIVERY_GATE;
    use super::CLOSE_PENDING;
    use super::CONNS;
    use super::CONTROLLED;
    use super::DELIVERY;
    use super::DELIVERY_FUTURE_PENDING;
    use super::DROP_MESSAGES;
    use super::IRREVOCABLE_SEND_GATE;
    use super::IRREVOCABLE_SEND_GATE_WAITING;
    use super::MAX_MESSAGE_SIZE;
    use super::NEXT_CALLBACK_CID;
    use super::NEXT_DELIVERY_GATE;
    use super::POST_PERMIT_SEND_GATE;
    use super::POST_PERMIT_SEND_GATE_WAITING;
    use super::SEND_MESSAGE_GATE;
    use super::SEND_MESSAGE_GATE_WAITING;
    use super::SEND_MESSAGE_PENDING;
    use super::SEND_MESSAGE_PENDING_AFTER_SENT_COUNT;
    use super::WAIT_FOR_DATA_CHANNEL_OPEN_PENDING;

    /// Atomic observation of the current thread's controlled delivery queue.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct DeliverySnapshot {
        pending: usize,
        generation: u64,
    }

    impl DeliverySnapshot {
        pub(super) const fn new(pending: usize, generation: u64) -> Self {
            Self {
                pending,
                generation,
            }
        }

        /// Return whether no controlled event is currently queued.
        pub const fn is_idle(self) -> bool {
            self.pending == 0
        }

        /// Return the number of controlled events currently queued.
        pub const fn pending(self) -> usize {
            self.pending
        }

        /// Return the queue generation, advanced on every enqueue or removal.
        pub const fn generation(self) -> u64 {
            self.generation
        }
    }

    /// Turn the controlled scheduler on/off for the current thread. Turning it
    /// off clears this thread's queue.
    pub fn enable(on: bool) {
        CONTROLLED.with(|c| c.set(on));
        if !on {
            DELIVERY.with(|state| state.borrow_mut().clear());
            super::SENT_COUNT.with(|count| count.set(0));
            MAX_MESSAGE_SIZE.with(|size| size.set(0));
            NEXT_CALLBACK_CID.with(|next| {
                *next.borrow_mut() = None;
            });
            WAIT_FOR_DATA_CHANNEL_OPEN_PENDING.with(|pending| pending.set(false));
            SEND_MESSAGE_PENDING.with(|pending| pending.set(false));
            release_send_message_gate();
            release_post_permit_send_gate();
            release_irrevocable_send_gate();
            SEND_MESSAGE_PENDING_AFTER_SENT_COUNT.with(|threshold| threshold.set(None));
            DELIVERY_FUTURE_PENDING.with(|pending| pending.set(false));
            CLOSE_PENDING.with(|pending| pending.set(false));
            release_delivery_future_gate();
            DROP_MESSAGES.with(|drop| drop.set(false));
        }
    }

    /// Test hook: override the `max_message_size` the dummy backend reports on this thread (`0`
    /// restores the default). Lets a test drive the chunked send path and reassembly end to end.
    pub fn set_max_message_size(n: usize) {
        MAX_MESSAGE_SIZE.with(|m| m.set(n));
    }

    /// Test hook: rewrite the next queued lifecycle callback to use `cid`.
    ///
    /// This applies only to peer-state and data-channel events delivered through
    /// [`deliver`]. Message events keep their real connection id.
    pub fn set_next_callback_cid(cid: impl Into<String>) {
        NEXT_CALLBACK_CID.with(|next| {
            *next.borrow_mut() = Some(cid.into());
        });
    }

    /// Test hook: force `webrtc_wait_for_data_channel_open` on this thread to never complete.
    pub fn set_wait_for_data_channel_open_pending(on: bool) {
        WAIT_FOR_DATA_CHANNEL_OPEN_PENDING.with(|pending| pending.set(on));
    }

    /// Test hook: force `send_message` to stay pending after the data channel is open.
    pub fn set_send_message_pending(on: bool) {
        SEND_MESSAGE_PENDING.with(|pending| pending.set(on));
    }

    /// Test hook: suspend the next dummy send immediately before dispatch.
    pub fn pause_send_message_at_dispatch() {
        SEND_MESSAGE_GATE.with(|gate| {
            *gate.borrow_mut() = Some(Arc::new(tokio::sync::Notify::new()));
        });
    }

    /// Test hook: release a send suspended by [`pause_send_message_at_dispatch`].
    pub fn release_send_message_gate() {
        let gate = SEND_MESSAGE_GATE.with(|gate| gate.borrow_mut().take());
        if let Some(gate) = gate {
            gate.notify_waiters();
        }
        SEND_MESSAGE_GATE_WAITING.with(|waiting| waiting.set(false));
    }

    /// Return whether a dummy send reached the releasable dispatch gate.
    pub fn send_message_waiting_at_dispatch() -> bool {
        SEND_MESSAGE_GATE_WAITING.with(|waiting| waiting.get())
    }

    /// Test hook: suspend the next dummy send after its initial permit check but
    /// before the final cancellable check.
    pub fn pause_send_message_after_permit() {
        POST_PERMIT_SEND_GATE.with(|gate| {
            *gate.borrow_mut() = Some(Arc::new(tokio::sync::Notify::new()));
        });
    }

    /// Test hook: release a send suspended before its final cancellable check.
    pub fn release_post_permit_send_gate() {
        let gate = POST_PERMIT_SEND_GATE.with(|gate| gate.borrow_mut().take());
        if let Some(gate) = gate {
            gate.notify_waiters();
        }
        POST_PERMIT_SEND_GATE_WAITING.with(|waiting| waiting.set(false));
    }

    /// Return whether a send is suspended before its final cancellable check.
    pub fn post_permit_send_gate_waiting() -> bool {
        POST_PERMIT_SEND_GATE_WAITING.with(|waiting| waiting.get())
    }

    /// Test hook: suspend the next dummy send after its final cancellable boundary.
    pub fn pause_irrevocable_send() {
        IRREVOCABLE_SEND_GATE.with(|gate| {
            *gate.borrow_mut() = Some(Arc::new(tokio::sync::Notify::new()));
        });
    }

    /// Test hook: release a send suspended after it became irrevocable.
    pub fn release_irrevocable_send_gate() {
        let gate = IRREVOCABLE_SEND_GATE.with(|gate| gate.borrow_mut().take());
        if let Some(gate) = gate {
            gate.notify_waiters();
        } else {
            IRREVOCABLE_SEND_GATE_WAITING.with(|waiting| waiting.set(false));
        }
    }

    /// Return whether a background dummy send is waiting past its irrevocable boundary.
    pub fn irrevocable_send_gate_waiting() -> bool {
        IRREVOCABLE_SEND_GATE_WAITING.with(|waiting| waiting.get())
    }

    /// Test hook: force `send_message` to stay pending once this thread has already dispatched
    /// `threshold` messages. `None` disables the hook.
    pub fn set_send_message_pending_after_sent_count(threshold: Option<usize>) {
        SEND_MESSAGE_PENDING_AFTER_SENT_COUNT.with(|pending_after| pending_after.set(threshold));
    }

    /// Test hook: make an accepted send return a delivery future that never completes.
    pub fn set_delivery_future_pending(on: bool) {
        DELIVERY_FUTURE_PENDING.with(|pending| pending.set(on));
    }

    /// Test hook: make connection cleanup never complete.
    pub fn set_close_pending(on: bool) {
        CLOSE_PENDING.with(|pending| pending.set(on));
    }

    /// Suspend exactly the next accepted send's delivery future.
    pub fn pause_next_delivery_future() {
        NEXT_DELIVERY_GATE.with(|slot| {
            *slot.borrow_mut() = Some(Arc::new(super::DeliveryGate::new()));
        });
    }

    /// Return whether the one-shot delivery future reached its gate.
    pub fn delivery_future_waiting() -> bool {
        ACTIVE_DELIVERY_GATE.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|gate| gate.waiting.load(super::Ordering::Acquire))
        })
    }

    /// Release a delivery future suspended by [`pause_next_delivery_future`].
    pub fn release_delivery_future_gate() {
        let gate = ACTIVE_DELIVERY_GATE
            .with(|slot| slot.borrow_mut().take())
            .or_else(|| NEXT_DELIVERY_GATE.with(|slot| slot.borrow_mut().take()));
        if let Some(gate) = gate {
            gate.notify.notify_one();
        }
    }

    /// Test hook: make dummy sends disappear while still returning a successful
    /// local send. This models a silent remote failure where the local data
    /// channel remains open and `Connected`.
    pub fn set_drop_messages(on: bool) {
        DROP_MESSAGES.with(|drop| drop.set(on));
    }

    /// Test hook: number of data-channel messages `send_message` has dispatched on this thread.
    /// Paired with [`reset_sent_count`] to assert that a failed send enqueued nothing.
    pub fn sent_count() -> usize {
        super::SENT_COUNT.with(|c| c.get())
    }

    /// Test hook: reset the [`sent_count`] counter for this thread.
    pub fn reset_sent_count() {
        super::SENT_COUNT.with(|c| c.set(0));
    }

    /// Number of events currently queued on the current thread.
    pub fn pending() -> usize {
        snapshot().pending()
    }

    /// Atomically observe queue depth and lifecycle generation on the current thread.
    pub fn snapshot() -> DeliverySnapshot {
        DELIVERY.with(|state| state.borrow().snapshot())
    }

    /// Deliver the queued event at `index` to its target connection — invoking
    /// the real handler, which may enqueue further events. Returns false if the
    /// index is out of range or the target connection is gone.
    pub async fn deliver(index: usize) -> bool {
        let entry = DELIVERY.with(|state| state.borrow_mut().remove(index));
        let Some((rand_id, mut event)) = entry else {
            return false;
        };
        let Some(conn) = CONNS.get(&rand_id).map(|c| c.clone()) else {
            return false;
        };
        if event.is_lifecycle_event() {
            if let Some(cid) = NEXT_CALLBACK_CID.with(|next| next.borrow_mut().take()) {
                event.set_callback_cid(cid);
            }
        }
        conn.handle_event(event).await;
        true
    }

    /// Deliver the next queued data-channel-open event with a rewritten callback cid.
    pub async fn deliver_next_data_channel_open_with_cid(cid: impl Into<String>) -> bool {
        let index = DELIVERY.with(|state| {
            state
                .borrow()
                .queue
                .iter()
                .position(|(_, event)| matches!(event, super::Event::DataChannelOpen(_)))
        });
        let Some(index) = index else {
            return false;
        };
        set_next_callback_cid(cid);
        deliver(index).await
    }
}

enum Event {
    PeerConnectionStateChange(WebrtcConnectionState, Option<String>),
    DataChannelOpen(Option<String>),
    DataChannelClose(Option<String>),
    Message(AdmittedInboundFrame),
}

impl Event {
    fn is_lifecycle_event(&self) -> bool {
        !matches!(self, Self::Message(_))
    }

    fn set_callback_cid(&mut self, cid: String) {
        match self {
            Self::PeerConnectionStateChange(_, callback_cid)
            | Self::DataChannelOpen(callback_cid)
            | Self::DataChannelClose(callback_cid) => {
                *callback_cid = Some(cid);
            }
            Self::Message(_) => {}
        }
    }
}

/// A dummy connection for local testing.
/// Implements the [ConnectionInterface] trait with no real network.
#[derive(Clone, Copy)]
struct DummyConnectionState {
    webrtc: WebrtcConnectionState,
    data_channel_open_override: Option<bool>,
}

impl DummyConnectionState {
    const fn snapshot(self) -> ConnectionStateSnapshot {
        let data_channel_open = match self.data_channel_open_override {
            Some(open) => open,
            None => matches!(
                self.webrtc,
                WebrtcConnectionState::Connected | WebrtcConnectionState::Connecting
            ),
        };
        ConnectionStateSnapshot::new(self.webrtc, data_channel_open)
    }
}

/// In-memory connection used by [`DummyTransport`] to model transport events.
pub struct DummyConnection {
    rand_id: String,
    callback: Arc<InnerTransportCallback>,
    event_sender: mpsc::UnboundedSender<Event>,
    remote_rand_id: Arc<Mutex<Option<String>>>,
    event_listener: JoinHandle<()>,
    connection_state: Arc<Mutex<DummyConnectionState>>,
    accepting_events: Arc<AtomicBool>,
    retirement_runtime: tokio::runtime::Handle,
}

/// [DummyTransport] manages all the [DummyConnection] and
/// provides methods to create, get and close connections.
pub struct DummyTransport {
    pool: Pool<DummyConnection>,
    inbound_frames: Arc<InboundFrameCapacity>,
}

impl DummyConnection {
    fn new(callback: InnerTransportCallback) -> Self {
        let rand_id = random(0, 10000000000).to_string();
        let retirement_runtime = tokio::runtime::Handle::current();

        let (tx, mut rx) = mpsc::unbounded_channel();

        let event_listener = {
            let rand_id = rand_id.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    // The connection may already have been closed and removed
                    // from the global map while events were still queued (a
                    // disconnect racing with close()/abort()). Stop draining
                    // instead of panicking on the missing entry.
                    let Some(conn) = CONNS.get(&rand_id).map(|c| c.clone()) else {
                        break;
                    };
                    conn.handle_event(ev).await;
                }
            })
        };

        Self {
            rand_id,
            callback: Arc::new(callback),
            event_sender: tx,
            remote_rand_id: Default::default(),
            event_listener,
            connection_state: Arc::new(Mutex::new(DummyConnectionState {
                webrtc: WebrtcConnectionState::New,
                data_channel_open_override: None,
            })),
            accepting_events: Arc::new(AtomicBool::new(true)),
            retirement_runtime,
        }
    }

    fn retirement_fence(&self) -> DummyRetirementFence {
        DummyRetirementFence::new(self)
    }

    async fn handle_event(&self, event: Event) {
        match event {
            Event::PeerConnectionStateChange(state, callback_cid) => {
                if let Some(cid) = callback_cid {
                    self.callback
                        .on_peer_connection_state_change_with_cid(&cid, state)
                        .await;
                } else {
                    self.callback.on_peer_connection_state_change(state).await;
                }
            }
            Event::DataChannelOpen(callback_cid) => {
                if let Some(cid) = callback_cid {
                    self.callback.on_data_channel_open_with_cid(&cid).await;
                } else {
                    self.callback.on_data_channel_open().await;
                }
            }
            Event::DataChannelClose(callback_cid) => {
                if let Some(cid) = callback_cid {
                    self.callback.on_data_channel_close_with_cid(&cid).await;
                } else {
                    self.callback.on_data_channel_close().await;
                }
            }
            Event::Message(frame) => {
                if SEND_MESSAGE_DELAY && !CONTROLLED.with(|c| c.get()) {
                    random_delay().await;
                }
                self.callback.handle_admitted_frame(frame).await
            }
        }
    }

    fn remote_rand_id(&self) -> MutexGuard<'_, Option<String>> {
        lock_recover(&self.remote_rand_id)
    }

    fn connection_state(&self) -> MutexGuard<'_, DummyConnectionState> {
        lock_recover(&self.connection_state)
    }

    fn remote_conn(&self) -> Option<Arc<DummyConnection>> {
        let cid = self.remote_rand_id().clone()?;
        // The remote may already have been closed and removed from the global
        // map (e.g. during a disconnect). Return None instead of panicking, so
        // callers treat it like a closed connection.
        CONNS.get(&cid).map(|c| c.clone())
    }

    fn set_remote_rand_id(&self, rand_id: String) {
        let mut remote_rand_id = self.remote_rand_id();
        *remote_rand_id = Some(rand_id);
    }

    /// Route an event to this connection's listener — or, when the test-only
    /// controlled scheduler is on, into [`DELIVERY`] for a test to deliver
    /// explicitly. Returns whether the event was accepted (the listener may be
    /// gone during teardown).
    fn dispatch(&self, event: Event) -> bool {
        if !self.accepting_events.load(Ordering::Acquire) {
            return false;
        }
        if CONTROLLED.with(|c| c.get()) {
            DELIVERY.with(|state| {
                state.borrow_mut().push_back((self.rand_id.clone(), event));
            });
            true
        } else {
            self.event_sender.send(event).is_ok()
        }
    }

    async fn set_webrtc_connection_state(&self, state: WebrtcConnectionState) {
        {
            let mut connection_state = self.connection_state();

            if state == connection_state.webrtc {
                return;
            }

            connection_state.webrtc = state;
        }

        self.dispatch(Event::PeerConnectionStateChange(state, None));

        if state == WebrtcConnectionState::Connected {
            self.dispatch(Event::DataChannelOpen(None));
        }

        if matches!(
            state,
            WebrtcConnectionState::Closed | WebrtcConnectionState::Disconnected
        ) {
            self.dispatch(Event::DataChannelClose(None));
        }
    }

    pub(crate) fn force_webrtc_connection_state_without_callback(
        &self,
        state: WebrtcConnectionState,
    ) {
        self.connection_state().webrtc = state;
    }

    pub(crate) fn force_data_channel_open_without_callback(&self, open: Option<bool>) {
        self.connection_state().data_channel_open_override = open;
    }
}

impl DummyTransport {
    /// Create a new [DummyTransport] instance.
    pub fn new(
        ice_servers: &str,
        _external_address: Option<String>,
        _udp_port_range: Option<WebrtcUdpPortRange>,
    ) -> Self {
        let _ = parse_ice_servers_or_warn(ice_servers, "dummy");

        Self {
            pool: Pool::new(),
            inbound_frames: Arc::new(InboundFrameCapacity::new()),
        }
    }
}

enum DummySendTarget {
    Deliver(Arc<DummyConnection>),
    Drop,
}

fn complete_irrevocable_send<F: FnOnce()>(
    connection_state: &Arc<Mutex<DummyConnectionState>>,
    data: Bytes,
    target: DummySendTarget,
    permit: IrrevocableSendGuard<F>,
) -> Result<DeliveryFuture> {
    commit_irrevocable_dispatch(connection_state, permit, || {
        match target {
            DummySendTarget::Deliver(remote) => {
                if let Some(frame) = remote.callback.prepare_inbound_frame(data) {
                    if !remote.dispatch(Event::Message(frame)) {
                        return Err(Error::DummyRemoteConnectionClosed);
                    }
                }
            }
            DummySendTarget::Drop => {}
        }
        SENT_COUNT.with(|count| count.set(count.get() + 1));
        Ok(())
    })?;
    if DELIVERY_FUTURE_PENDING.with(|pending| pending.get()) {
        return Ok(Box::pin(std::future::pending::<Result<()>>()));
    }
    let delivery_gate = NEXT_DELIVERY_GATE.with(|slot| slot.borrow_mut().take());
    if let Some(gate) = delivery_gate {
        ACTIVE_DELIVERY_GATE.with(|slot| {
            *slot.borrow_mut() = Some(gate.clone());
        });
        return Ok(Box::pin(async move {
            gate.waiting.store(true, Ordering::Release);
            gate.notify.notified().await;
            gate.waiting.store(false, Ordering::Release);
            Ok(())
        }));
    }
    Ok(Box::pin(async { Ok(()) }))
}

fn commit_irrevocable_dispatch<F, T>(
    connection_state: &Arc<Mutex<DummyConnectionState>>,
    permit: IrrevocableSendGuard<F>,
    dispatch: impl FnOnce() -> Result<T>,
) -> Result<T>
where
    F: FnOnce(),
{
    let state = lock_recover(connection_state);
    if !state.snapshot().data_channel_open() {
        drop(state);
        return Err(Error::DummyConnectionRetiredBeforeDispatch);
    }
    match dispatch() {
        Ok(value) => {
            permit.mark_accepted();
            drop(state);
            Ok(value)
        }
        Err(error) => {
            drop(state);
            Err(error)
        }
    }
}

#[async_trait]
impl ConnectionInterface for DummyConnection {
    type Sdp = String;
    type Error = Error;

    async fn send_message_with_permit(
        &self,
        msg: TransportMessage,
        permit: SendPermit,
    ) -> Result<DeliveryFuture> {
        self.webrtc_wait_for_data_channel_open().await?;
        if SEND_MESSAGE_PENDING.with(|pending| pending.get())
            || SEND_MESSAGE_PENDING_AFTER_SENT_COUNT.with(|threshold| {
                threshold
                    .get()
                    .map(|count| SENT_COUNT.with(|sent| sent.get()) >= count)
                    .unwrap_or(false)
            })
        {
            std::future::pending::<()>().await;
        }
        let send_gate = SEND_MESSAGE_GATE.with(|gate| gate.borrow().clone());
        if let Some(send_gate) = send_gate {
            SEND_MESSAGE_GATE_WAITING.with(|waiting| waiting.set(true));
            send_gate.notified().await;
            SEND_MESSAGE_GATE_WAITING.with(|waiting| waiting.set(false));
        }
        let data = rings_codec::serialize(&msg).map(Bytes::from)?;
        if !permit.allows() {
            return Err(Error::SendPermitRevoked);
        }
        let post_permit_gate = POST_PERMIT_SEND_GATE.with(|gate| gate.borrow().clone());
        if let Some(post_permit_gate) = post_permit_gate {
            POST_PERMIT_SEND_GATE_WAITING.with(|waiting| waiting.set(true));
            post_permit_gate.notified().await;
            POST_PERMIT_SEND_GATE_WAITING.with(|waiting| waiting.set(false));
        }
        if !permit.allows() {
            return Err(Error::SendPermitRevoked);
        }
        let target = if DROP_MESSAGES.with(|drop| drop.get()) {
            DummySendTarget::Drop
        } else {
            DummySendTarget::Deliver(
                self.remote_conn()
                    .ok_or(Error::DummyRemoteConnectionUnavailable)?,
            )
        };
        let retirement_fence = self.retirement_fence();
        let mut permit_retirement =
            IrrevocableSendGuard::new(permit.acceptance(), move || retirement_fence.request());
        let Some(proof) = permit.try_mark_irrevocable() else {
            return Err(Error::SendPermitRevoked);
        };
        permit_retirement.bind(proof);
        let irrevocable_gate = IRREVOCABLE_SEND_GATE.with(|gate| gate.borrow().clone());
        if let Some(irrevocable_gate) = irrevocable_gate {
            let connection_state = Arc::clone(&self.connection_state);
            let (result_sender, result_receiver) = oneshot::channel();
            tokio::spawn(async move {
                IRREVOCABLE_SEND_GATE_WAITING.with(|waiting| waiting.set(true));
                irrevocable_gate.notified().await;
                IRREVOCABLE_SEND_GATE_WAITING.with(|waiting| waiting.set(false));
                let result =
                    complete_irrevocable_send(&connection_state, data, target, permit_retirement);
                let _ = result_sender.send(result);
            });
            return result_receiver
                .await
                .map_err(|_| Error::DummyIrrevocableSendTaskStopped)?;
        }
        complete_irrevocable_send(&self.connection_state, data, target, permit_retirement)
    }

    fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        self.connection_state().webrtc
    }

    fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
        self.connection_state().snapshot()
    }

    fn data_channel_is_open(&self) -> Result<bool> {
        Ok(self.connection_state_snapshot().data_channel_open())
    }

    fn max_message_size(&self) -> usize {
        stored_max_message_size(MAX_MESSAGE_SIZE.with(std::cell::Cell::get))
    }

    async fn get_stats(&self) -> Vec<String> {
        Vec::new()
    }

    async fn webrtc_create_offer(&self) -> Result<Self::Sdp> {
        self.set_webrtc_connection_state(WebrtcConnectionState::New)
            .await;
        Ok(self.rand_id.clone())
    }

    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp> {
        // Set remote rand id before setting state so that the remote connection can be found in callback.
        self.set_remote_rand_id(offer);
        self.set_webrtc_connection_state(WebrtcConnectionState::Connecting)
            .await;
        Ok(self.rand_id.clone())
    }

    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<()> {
        // Set remote rand id before setting state so that the remote connection can be found in callback.
        self.set_remote_rand_id(answer);
        self.set_webrtc_connection_state(WebrtcConnectionState::Connected)
            .await;

        if let Some(remote_conn) = self.remote_conn() {
            remote_conn
                .set_webrtc_connection_state(WebrtcConnectionState::Connected)
                .await;
        }

        Ok(())
    }

    async fn webrtc_wait_for_data_channel_open(&self) -> Result<()> {
        if WAIT_FOR_DATA_CHANNEL_OPEN_PENDING.with(|pending| pending.get()) {
            std::future::pending::<()>().await;
        }
        if self.data_channel_is_open()? {
            Ok(())
        } else {
            Err(Error::DataChannelOpen(
                "State is not connected in dummy connection".to_string(),
            ))
        }
    }

    async fn close(&self) -> Result<()> {
        let retirement = self.retirement_fence().begin();
        if CLOSE_PENDING.with(|pending| pending.get()) {
            std::future::pending::<()>().await;
        }
        retirement.finish();
        Ok(())
    }
}

#[async_trait]
impl TransportInterface for DummyTransport {
    type Connection = DummyConnection;
    type Error = Error;

    fn inbound_frame_capacity(&self) -> &Arc<InboundFrameCapacity> {
        &self.inbound_frames
    }

    async fn new_connection(
        &self,
        cid: &str,
        callback: BoxedTransportCallback,
    ) -> Result<ConnectionRef<Self::Connection>> {
        self.pool.ensure_peer_slot_available(cid)?;

        let inner_callback =
            InnerTransportCallback::for_transport(self, cid, callback, Notifier::default());
        let conn = DummyConnection::new(inner_callback);

        let connection = self.pool.safely_insert(cid, conn).await?;
        let conn = connection.upgrade()?;
        CONNS.insert(conn.rand_id.clone(), conn);

        Ok(connection)
    }

    async fn close_connection(&self, cid: &str) -> Result<()> {
        self.pool.safely_remove(cid).await
    }

    async fn close_connection_if_current(
        &self,
        connection: &ConnectionRef<Self::Connection>,
    ) -> Result<bool> {
        self.pool.safely_remove_if_current(connection).await
    }

    fn connection(&self, cid: &str) -> Result<ConnectionRef<Self::Connection>> {
        self.pool.connection(cid)
    }

    fn connections(&self) -> Vec<(String, ConnectionRef<Self::Connection>)> {
        self.pool.connections()
    }

    fn connection_ids(&self) -> Vec<String> {
        self.pool.connection_ids()
    }
}
