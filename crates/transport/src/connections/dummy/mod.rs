use std::any::Any;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicUsize;
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
use tokio::sync::watch;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::callback::link_send::FrameSink;
use crate::callback::InnerTransportCallback;
use crate::callback::NodeFramePool;
use crate::connection_pool::Pool;
use crate::connection_ref::ConnectionRef;
use crate::connections::PhysicalCloseWitness;
use crate::core::callback::BoxedTransportCallback;
use crate::core::credit::CreditIndex;
use crate::core::pool::ChannelLane;
use crate::core::transport::stored_max_message_size;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateSnapshot;
use crate::core::transport::HandshakeStep;
use crate::core::transport::IrrevocableSendGuard;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportConnectionState;
use crate::core::transport::TransportInterface;
use crate::core::transport::TransportMessage;
use crate::delivery::DeliveryFuture;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::parse_ice_servers_or_warn;
use crate::ice_server::parse_ice_servers_strict;
use crate::ice_server::IceServer;
use crate::network_config::UdpPortRange;
use crate::notifier::Notifier;
use crate::sdp::effective_max_message_size;
use crate::sync_utils::lock_recover;

/// Deterministic and randomized scheduler yield helpers for dummy arrival jitter.
mod delay;
mod event;
/// Dummy connection retirement fence and cleanup guards.
mod retirement;
mod state;

use self::delay::random;
use self::delay::yield_before_arrival;
use self::event::Event;
use self::retirement::DummyRetirementFence;
use self::state::set_parked;
use self::state::ControlledDeliveryEntry;
use self::state::ACTIVE_DELIVERY_GATE;
use self::state::CLOSE_PENDING;
use self::state::CONTROLLED;
use self::state::CONTROLLED_LINKS;
use self::state::CONTROLLED_RNG_STATE;
use self::state::CONTROLLED_VIRTUAL_MS;
use self::state::DELIVERY;
use self::state::DELIVERY_ENQUEUED;
use self::state::DELIVERY_FUTURE_PENDING;
use self::state::DROP_MESSAGES;
use self::state::HELD_DELIVERY_GATE;
use self::state::IRREVOCABLE_SEND_GATE;
use self::state::IRREVOCABLE_SEND_GATE_PARKED;
use self::state::MAX_MESSAGE_SIZE;
use self::state::NEXT_CALLBACK_CID;
use self::state::NEXT_DELIVERY_GATE;
use self::state::POST_PERMIT_SEND_GATE;
use self::state::POST_PERMIT_SEND_GATE_PARKED;
use self::state::SEND_MESSAGE_GATE;
use self::state::SEND_MESSAGE_GATE_PARKED;
use self::state::SEND_MESSAGE_PENDING;
use self::state::SEND_MESSAGE_PENDING_AFTER_SENT_COUNT;
use self::state::SENT_COUNT;
use self::state::WAIT_FOR_DATA_CHANNEL_OPEN_PENDING;
use self::state::WITHHELD_CREDIT;

/// Exclusive bound on the scheduler turns a frame waits before it arrives
/// (see [`yield_before_arrival`]).
const ARRIVAL_TURNS: u64 = 16;

lazy_static! {
    static ref CONNS: DashMap<String, Arc<DummyConnection>> = DashMap::new();
}

/// One-shot test gate that parks a single dummy delivery future until released.
struct DeliveryGate {
    /// Whether a delivery future is currently parked on `notify`.
    waiting: AtomicBool,
    /// Notification primitive woken by tests to release the parked delivery.
    notify: Notify,
}

impl DeliveryGate {
    /// Construct a new unparked [`DeliveryGate`].
    fn new() -> Self {
        Self {
            waiting: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }
}

/// A completion gate shared by every delivery accepted while it is installed.
///
/// Unlike the one-shot [`DeliveryGate`], it holds any number of deliveries until one release,
/// so a test can keep a whole in-flight window pending.
struct HeldDeliveries {
    /// Set once by the release; a delivery that observes it completes.
    released: AtomicBool,
    /// Deliveries allowed to complete one by one before the release.
    permits: AtomicUsize,
    /// Deliveries currently parked on the gate.
    waiting: AtomicUsize,
    /// Wakes every parked delivery on release.
    notify: Notify,
}

impl HeldDeliveries {
    /// A gate holding every delivery until released.
    fn new() -> Self {
        Self {
            released: AtomicBool::new(false),
            permits: AtomicUsize::new(0),
            waiting: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    /// Park until the release. The waiter registers before it checks the flag, so a release
    /// between the check and the wait is not lost.
    async fn wait(&self) {
        self.waiting.fetch_add(1, Ordering::AcqRel);
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.released.load(Ordering::Acquire) || self.take_permit() {
                break;
            }
            notified.await;
        }
        self.waiting.fetch_sub(1, Ordering::AcqRel);
    }

    /// Consume one single-delivery permit, if one is left.
    fn take_permit(&self) -> bool {
        self.permits
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |permits| {
                permits.checked_sub(1)
            })
            .is_ok()
    }

    /// Let exactly one parked (or the next) delivery complete.
    fn release_one(&self) {
        self.permits.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_waiters();
    }

    /// Release every parked and future delivery of this gate.
    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod test_dummy;

/// Test-only controlled delivery scheduler. When enabled (per thread), dummy
/// message/event delivery is queued instead of auto-dispatched, so a test can
/// drive the exact ordering and deterministically explore the timing-state space
/// (see `rings_core`'s `tests::default::test_dht_schedule`). Off by default; no effect
/// on normal runs.
pub mod controlled;

/// A dummy connection for local testing.
/// Implements the [ConnectionInterface] trait with no real network.
#[derive(Clone, Copy)]
struct DummyConnectionState {
    /// Simulated WebRTC connection phase.
    webrtc: TransportConnectionState,
    /// Optional explicit override for data-channel openness used in callback-ordering tests.
    data_channel_open_override: Option<bool>,
}

impl DummyConnectionState {
    /// The product state the transport observes.
    ///
    /// Law (causality): without an override the data channel is open iff the peer connection is
    /// `Connected`, and a dummy connection reaches `Connected` only when the offerer accepts the
    /// answer, which moves both ends at once. An answerer is therefore never open, and never
    /// admitted, before the offerer has applied its answer; a real WebRTC channel cannot open
    /// earlier either. Tests that model the browser callback order in which the channel opens
    /// while the peer connection still reports `Connecting` set the override explicitly.
    const fn snapshot(self) -> ConnectionStateSnapshot {
        let data_channel_open = match self.data_channel_open_override {
            Some(open) => open,
            None => matches!(self.webrtc, TransportConnectionState::Connected),
        };
        ConnectionStateSnapshot::new(self.webrtc, data_channel_open)
    }
}

/// Handshake role before any offer or answer has been created or accepted.
const HANDSHAKE_ROLE_FRESH: u8 = 0;
/// Handshake role after creating a local offer while awaiting a remote answer.
const HANDSHAKE_ROLE_INITIATOR_AWAITING_ANSWER: u8 = 1;
/// Handshake role once the offer/answer exchange has completed.
const HANDSHAKE_ROLE_ESTABLISHED: u8 = 2;
/// Transient handshake role held while a handshake step is executing.
const HANDSHAKE_ROLE_IN_PROGRESS: u8 = 3;
/// Terminal handshake role entered if a handshake step fails or is cancelled mid-flight.
const HANDSHAKE_ROLE_FAILED: u8 = 4;

/// RAII guard that transitions a connection's `handshake_role` to [`HANDSHAKE_ROLE_FAILED`] on drop unless committed.
struct HandshakeRoleGuard<'a> {
    /// Atomic handshake role cell being guarded.
    role: &'a AtomicU8,
    /// Whether [`Self::commit`] has stored the next role.
    committed: bool,
}

impl<'a> HandshakeRoleGuard<'a> {
    /// Guard `role` until [`Self::commit`] is called.
    fn new(role: &'a AtomicU8) -> Self {
        Self {
            role,
            committed: false,
        }
    }

    /// Store `next_role` and disarm the failure fallback on drop.
    fn commit(mut self, next_role: u8) {
        self.role.store(next_role, Ordering::Release);
        self.committed = true;
    }
}

impl Drop for HandshakeRoleGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.role.store(HANDSHAKE_ROLE_FAILED, Ordering::Release);
        }
    }
}

/// In-memory connection used by [`DummyTransport`] to model transport events.
pub struct DummyConnection {
    /// Unique random identifier indexing this connection in `CONNS`.
    rand_id: String,
    /// Callback wrapper handling credit flow control and protocol dispatch.
    callback: Arc<InnerTransportCallback>,
    /// Unbounded channel delivering events to the background event listener task.
    event_sender: mpsc::UnboundedSender<Event>,
    /// Random identifier of the paired remote dummy connection, once linked.
    remote_rand_id: Arc<Mutex<Option<String>>>,
    /// Negotiated remote maximum data-channel message size, or `0` if unset.
    remote_max_message_size: AtomicUsize,
    /// Atomic handshake state encoded using the `HANDSHAKE_ROLE_*` constants.
    handshake_role: AtomicU8,
    /// Background task draining `event_sender` in uncontrolled mode.
    event_listener: JoinHandle<()>,
    /// Synchronized simulated WebRTC state and channel-openness override.
    connection_state: Arc<Mutex<DummyConnectionState>>,
    /// Gate cleared on retirement to stop accepting new events.
    accepting_events: Arc<AtomicBool>,
    /// Monotone watch sender signaled once physical retirement cleanup finishes.
    physical_close_completed: Arc<watch::Sender<bool>>,
    /// Tokio runtime handle used to spawn retirement notifications.
    retirement_runtime: tokio::runtime::Handle,
    /// The payload frames of this connection's transport; see
    /// [`DummyTransport::dispatched_payload_frames`].
    dispatched: Arc<AtomicUsize>,
}

/// [DummyTransport] manages all the [DummyConnection] and
/// provides methods to create, get and close connections.
pub struct DummyTransport {
    /// Raw ICE server string paired with its parsed [`IceServer`] list.
    ice_state: std::sync::RwLock<(String, Vec<IceServer>)>,
    /// Active dummy connections keyed by peer connection ID.
    pool: Pool<DummyConnection>,
    /// The frames this node lends its connections' lanes, shared by every connection.
    frame_pool: NodeFramePool,
    /// The payload frames every connection of this transport has dispatched; see
    /// [`Self::dispatched_payload_frames`].
    dispatched: Arc<AtomicUsize>,
}

impl DummyConnection {
    /// Return this dummy connection's unique generation identifier (`rand_id`).
    pub(crate) fn generation_id(&self) -> &str {
        &self.rand_id
    }

    /// Subscribe a monotone [`PhysicalCloseWitness`] that resolves once this connection's
    /// retirement finishes.
    pub fn physical_close_witness(&self) -> PhysicalCloseWitness {
        PhysicalCloseWitness::new(self.physical_close_completed.subscribe())
    }

    /// A connection that counts the payload frames it dispatches in `dispatched`, its transport's
    /// count.
    fn new(callback: InnerTransportCallback, dispatched: Arc<AtomicUsize>) -> Self {
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
            remote_max_message_size: AtomicUsize::new(0),
            handshake_role: AtomicU8::new(HANDSHAKE_ROLE_FRESH),
            event_listener,
            connection_state: Arc::new(Mutex::new(DummyConnectionState {
                webrtc: TransportConnectionState::New,
                data_channel_open_override: None,
            })),
            accepting_events: Arc::new(AtomicBool::new(true)),
            physical_close_completed: Arc::new(watch::channel(false).0),
            retirement_runtime,
            dispatched,
        }
    }

    /// Start this connection's credit pumps, one per lane: every signal its lanes queue (a
    /// credit or shrink of its receive side, a return of its send side) is applied to the paired
    /// connection directly, the dummy link carrying signals without latency. A signal always
    /// answers a frame or a signal of the paired connection, so it has one to go to; once that
    /// connection is gone its ends are gone with it, and the signal has no one to reach. The
    /// dummy link runs no wall-clock idle check: the controlled scheduler's virtual clock drives
    /// it (`controlled::set_virtual_time`), and a test without one drives it by event
    /// (`LinkCredit::idle_check`), so no timer changes a deterministic schedule.
    /// The pumps stop when this connection's callback is dropped. Post: `false` when a pump
    /// cannot be spawned.
    fn spawn_credit_pumps(self: &Arc<Self>) -> bool {
        CreditIndex::ALL.into_iter().all(|index| {
            let link = Arc::clone(self.callback.link_credit());
            let connection = Arc::downgrade(self);
            let pump = async move {
                while let Some(signal) = link.next_signal(index).await {
                    let Some(conn) = connection.upgrade() else {
                        continue;
                    };
                    if WITHHELD_CREDIT.with(|withheld| withheld.borrow().contains(&conn.rand_id)) {
                        continue;
                    }
                    if let Some(remote) = conn.remote_conn() {
                        remote.callback.link_credit().apply(index.lane(), signal);
                    }
                }
            };
            rings_runtime::spawn_detached(pump).is_ok()
        })
    }

    /// Capture a [`DummyRetirementFence`] for retiring this connection generation.
    fn retirement_fence(&self) -> DummyRetirementFence {
        DummyRetirementFence::new(self)
    }

    /// Deliver one internal [`Event`] to this connection's callback.
    async fn handle_event(&self, event: Event) {
        match event {
            Event::ConnectionStateChange(state, callback_cid) => {
                if let Some(cid) = callback_cid {
                    self.callback
                        .on_connection_state_change_with_cid(&cid, state)
                        .await;
                } else {
                    self.callback.on_connection_state_change(state).await;
                }
            }
            Event::ChannelOpen(callback_cid) => {
                if let Some(cid) = callback_cid {
                    self.callback.on_channel_open_with_cid(&cid).await;
                } else {
                    self.callback.on_channel_open().await;
                }
            }
            Event::ChannelClose(callback_cid) => {
                if let Some(cid) = callback_cid {
                    self.callback.on_channel_close_with_cid(&cid).await;
                } else {
                    self.callback.on_channel_close().await;
                }
            }
            Event::Message(frame, lane) => {
                if CONTROLLED.with(|c| c.get()) {
                    // The deterministic scheduler delivers one event at a time, and a delivery
                    // means the event was processed: the lane is drained inline, the
                    // interleaving in which its drainer runs at once.
                    self.callback
                        .dispatch_admitted_frame_inline(frame, lane)
                        .await;
                    return;
                }
                yield_before_arrival().await;
                // Arrival never waits on the protocol: the frame joins its lane's FIFO, as a
                // native or browser channel's frames do (see `InnerTransportCallback`).
                self.callback.dispatch_admitted_frame(frame, lane);
            }
        }
    }

    /// Lock the optional paired remote connection identifier.
    fn remote_rand_id(&self) -> MutexGuard<'_, Option<String>> {
        lock_recover(&self.remote_rand_id)
    }

    /// Lock the simulated connection state.
    fn connection_state(&self) -> MutexGuard<'_, DummyConnectionState> {
        lock_recover(&self.connection_state)
    }

    /// Look up the paired remote [`DummyConnection`] in `CONNS`, if still registered.
    fn remote_conn(&self) -> Option<Arc<DummyConnection>> {
        let cid = self.remote_rand_id().clone()?;
        // The remote may already have been closed and removed from the global
        // map (e.g. during a disconnect). Return None instead of panicking, so
        // callers treat it like a closed connection.
        CONNS.get(&cid).map(|c| c.clone())
    }

    /// Record the paired remote connection's `rand_id` extracted from its first line.
    fn set_remote_rand_id(&self, rand_id: String) {
        let peer_id = rand_id
            .lines()
            .next()
            .unwrap_or(rand_id.as_str())
            .to_string();
        let mut remote_rand_id = self.remote_rand_id();
        *remote_rand_id = Some(peer_id);
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

    /// Update the simulated connection phase and dispatch lifecycle events if it changed.
    async fn set_connection_state(&self, state: TransportConnectionState) {
        {
            let mut connection_state = self.connection_state();

            if state == connection_state.webrtc {
                return;
            }

            connection_state.webrtc = state;
        }

        self.dispatch(Event::ConnectionStateChange(state, None));

        if state == TransportConnectionState::Connected {
            self.dispatch(Event::ChannelOpen(None));
        }

        if matches!(
            state,
            TransportConnectionState::Closed | TransportConnectionState::Disconnected
        ) {
            self.dispatch(Event::ChannelClose(None));
        }
    }

    /// Test hook: overwrite the simulated WebRTC state without emitting callbacks.
    pub(crate) fn force_connection_state_without_callback(&self, state: TransportConnectionState) {
        self.connection_state().webrtc = state;
    }

    /// Test hook: overwrite the simulated data-channel openness without emitting callbacks.
    pub(crate) fn force_data_channel_open_without_callback(&self, open: Option<bool>) {
        self.connection_state().data_channel_open_override = open;
    }
}

impl DummyTransport {
    /// Create a new [DummyTransport] instance, logging and ignoring invalid ICE configuration.
    pub fn new(
        ice_servers: &str,
        _external_address: Option<String>,
        _udp_port_range: Option<UdpPortRange>,
    ) -> Self {
        let parsed = parse_ice_servers_or_warn(ice_servers, "dummy");

        Self {
            ice_state: std::sync::RwLock::new((ice_servers.to_string(), parsed)),
            pool: Pool::new(),
            frame_pool: NodeFramePool::new(),
            dispatched: Arc::default(),
        }
    }

    /// Create a new [DummyTransport] instance, failing closed when `ice_servers` is malformed.
    pub fn new_strict(
        ice_servers: &str,
        _external_address: Option<String>,
        _udp_port_range: Option<UdpPortRange>,
    ) -> Result<Self> {
        let parsed = parse_ice_servers_strict(ice_servers)?;
        Ok(Self {
            ice_state: std::sync::RwLock::new((ice_servers.to_string(), parsed)),
            pool: Pool::new(),
            frame_pool: NodeFramePool::new(),
            dispatched: Arc::default(),
        })
    }

    /// Validate and replace the ICE server list used for subsequent peer connections.
    pub fn set_ice_servers(&self, ice_servers: &str) {
        let parsed = parse_ice_servers_or_warn(ice_servers, "dummy");
        self.commit_prepared_ice_servers(ice_servers.to_string(), parsed);
    }

    /// Strictly validate and replace the ICE server list, returning [`Error::IceServer`] without
    /// mutating state if `ice_servers` is malformed.
    pub fn try_set_ice_servers(&self, ice_servers: &str) -> Result<()> {
        let parsed = parse_ice_servers_strict(ice_servers)?;
        self.commit_prepared_ice_servers(ice_servers.to_string(), parsed);
        Ok(())
    }

    /// Atomically commit a pre-validated `(raw_credentials, parsed_ice_servers)` pair.
    pub fn commit_prepared_ice_servers(&self, raw: String, parsed: Vec<IceServer>) {
        *self
            .ice_state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (raw, parsed);
    }

    /// Return the currently committed raw session credential / ICE server string.
    pub fn session_credentials(&self) -> String {
        self.ice_state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
            .clone()
    }

    /// Test hook: the payload frames this transport's connections have dispatched, over every
    /// connection it has opened.
    ///
    /// Unlike [`controlled::sent_count`], which counts every node on the thread, this counts
    /// only the frames this node sent, so a test can count its own sends while its peers
    /// answer them.
    pub fn dispatched_payload_frames(&self) -> usize {
        self.dispatched.load(Ordering::Acquire)
    }
}

/// What one dispatched frame counts toward: a payload frame counts toward its transport's
/// [`DummyTransport::dispatched_payload_frames`] and its thread's [`controlled::sent_count`].
/// A credit frame counts toward neither: a receiver's credit pump sends it on its own, so a
/// test's counts never depend on when a peer advertises credit.
enum FrameCount {
    /// A payload frame, counted toward the transport that owns the counter.
    Payload(Arc<AtomicUsize>),
    /// A credit frame.
    Credit,
}

/// Disposition of an irrevocable dummy send after crossing the permit boundary.
enum DummySendTarget {
    /// Admit and enqueue the frame onto the paired remote dummy connection.
    Deliver(Arc<DummyConnection>),
    /// Simulate a dropped frame without delivering it to the remote connection.
    Drop,
}

/// Commit an irrevocable dummy send under `connection_state` and return its [`DeliveryFuture`].
fn complete_irrevocable_send<F: FnOnce()>(
    connection_state: &Arc<Mutex<DummyConnectionState>>,
    data: Bytes,
    lane: ChannelLane,
    target: DummySendTarget,
    permit: IrrevocableSendGuard<F>,
    count: FrameCount,
) -> Result<DeliveryFuture> {
    commit_irrevocable_dispatch(connection_state, permit, || {
        match target {
            DummySendTarget::Deliver(remote) => {
                if let Some(frame) = remote.callback.prepare_inbound_frame(data, lane) {
                    if !remote.dispatch(Event::Message(frame, lane)) {
                        return Err(Error::DummyRemoteConnectionClosed);
                    }
                }
            }
            DummySendTarget::Drop => {}
        }
        if let FrameCount::Payload(dispatched) = count {
            dispatched.fetch_add(1, Ordering::AcqRel);
            SENT_COUNT.with(|sent| sent.set(sent.get() + 1));
        }
        Ok(())
    })?;
    if DELIVERY_FUTURE_PENDING.with(|pending| pending.get()) {
        return Ok(Box::pin(std::future::pending::<Result<()>>()));
    }
    if let Some(gate) = HELD_DELIVERY_GATE.with(|slot| slot.borrow().clone()) {
        return Ok(Box::pin(async move {
            gate.wait().await;
            Ok(())
        }));
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

/// Verify the dummy channel is still open, run `dispatch`, and mark `permit` accepted on success.
fn commit_irrevocable_dispatch<F, T>(
    connection_state: &Arc<Mutex<DummyConnectionState>>,
    permit: IrrevocableSendGuard<F>,
    dispatch: impl FnOnce() -> Result<T>,
) -> Result<T>
where
    F: FnOnce(),
{
    let state = lock_recover(connection_state);
    if !state.snapshot().channel_open() {
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
impl FrameSink for DummyConnection {
    /// The dummy link delivers one connection's messages over one ordered event queue, which
    /// meets the lane law for every lane at once (a controlled test may reorder events on
    /// purpose), so the lane needs no channel of its own here.
    async fn send_frame(
        &self,
        frame: TransportMessage,
        lane: ChannelLane,
        permit: SendPermit,
    ) -> Result<DeliveryFuture> {
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
            set_parked(&SEND_MESSAGE_GATE_PARKED, true);
            send_gate.notified().await;
            set_parked(&SEND_MESSAGE_GATE_PARKED, false);
        }
        let count = match frame {
            TransportMessage::Custom(_) => FrameCount::Payload(Arc::clone(&self.dispatched)),
            TransportMessage::Signal(_) => FrameCount::Credit,
        };
        let data = rings_codec::serialize(&frame).map(Bytes::from)?;
        if !permit.allows() {
            return Err(Error::SendPermitRevoked);
        }
        let post_permit_gate = POST_PERMIT_SEND_GATE.with(|gate| gate.borrow().clone());
        if let Some(post_permit_gate) = post_permit_gate {
            set_parked(&POST_PERMIT_SEND_GATE_PARKED, true);
            post_permit_gate.notified().await;
            set_parked(&POST_PERMIT_SEND_GATE_PARKED, false);
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
                set_parked(&IRREVOCABLE_SEND_GATE_PARKED, true);
                irrevocable_gate.notified().await;
                set_parked(&IRREVOCABLE_SEND_GATE_PARKED, false);
                let result = complete_irrevocable_send(
                    &connection_state,
                    data,
                    lane,
                    target,
                    permit_retirement,
                    count,
                );
                let _ = result_sender.send(result);
            });
            return result_receiver
                .await
                .map_err(|_| Error::DummyIrrevocableSendTaskStopped)?;
        }
        complete_irrevocable_send(
            &self.connection_state,
            data,
            lane,
            target,
            permit_retirement,
            count,
        )
    }
}

#[async_trait]
impl ConnectionInterface for DummyConnection {
    type Error = Error;

    delegate_to_callback!();

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
        self.connection_state().snapshot()
    }

    fn max_message_size(&self) -> usize {
        let override_size = MAX_MESSAGE_SIZE.with(std::cell::Cell::get);
        if override_size == 0 {
            stored_max_message_size(self.remote_max_message_size.load(Ordering::Acquire))
        } else {
            override_size
        }
    }

    async fn handshake_step(&self, inbound: Option<Bytes>) -> Result<HandshakeStep> {
        match inbound {
            None => {
                self.handshake_role
                    .compare_exchange(
                        HANDSHAKE_ROLE_FRESH,
                        HANDSHAKE_ROLE_IN_PROGRESS,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .map_err(|_| {
                        Error::LocalHandshakeGenerationError(
                            "unexpected initial handshake step on active dummy connection"
                                .to_string(),
                        )
                    })?;
                let role_guard = HandshakeRoleGuard::new(&self.handshake_role);
                self.set_connection_state(TransportConnectionState::New)
                    .await;
                role_guard.commit(HANDSHAKE_ROLE_INITIATOR_AWAITING_ANSWER);
                Ok(HandshakeStep::Send(Bytes::from(
                    self.rand_id.clone().into_bytes(),
                )))
            }
            Some(inbound_bytes) => {
                if self
                    .handshake_role
                    .compare_exchange(
                        HANDSHAKE_ROLE_FRESH,
                        HANDSHAKE_ROLE_IN_PROGRESS,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    let role_guard = HandshakeRoleGuard::new(&self.handshake_role);
                    let offer = std::str::from_utf8(&inbound_bytes)
                        .map_err(|err| {
                            Error::RemoteHandshakeApplyError(format!(
                                "invalid UTF-8 handshake payload: {err}"
                            ))
                        })?
                        .to_owned();
                    let negotiated_max_message_size = effective_max_message_size(&offer)?;
                    // Set remote rand id before setting state so that the remote connection can be found in callback.
                    self.set_remote_rand_id(offer);
                    self.set_connection_state(TransportConnectionState::Connecting)
                        .await;
                    self.remote_max_message_size
                        .store(negotiated_max_message_size, Ordering::Release);
                    role_guard.commit(HANDSHAKE_ROLE_ESTABLISHED);
                    return Ok(HandshakeStep::SendFinal(Bytes::from(
                        self.rand_id.clone().into_bytes(),
                    )));
                }

                if self
                    .handshake_role
                    .compare_exchange(
                        HANDSHAKE_ROLE_INITIATOR_AWAITING_ANSWER,
                        HANDSHAKE_ROLE_IN_PROGRESS,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    let role_guard = HandshakeRoleGuard::new(&self.handshake_role);
                    let answer = std::str::from_utf8(&inbound_bytes)
                        .map_err(|err| {
                            Error::RemoteHandshakeApplyError(format!(
                                "invalid UTF-8 handshake payload: {err}"
                            ))
                        })?
                        .to_owned();
                    let negotiated_max_message_size = effective_max_message_size(&answer)?;
                    // Set remote rand id before setting state so that the remote connection can be found in callback.
                    self.set_remote_rand_id(answer);
                    self.remote_max_message_size
                        .store(negotiated_max_message_size, Ordering::Release);
                    self.set_connection_state(TransportConnectionState::Connected)
                        .await;

                    if let Some(remote_conn) = self.remote_conn() {
                        remote_conn
                            .set_connection_state(TransportConnectionState::Connected)
                            .await;
                    }
                    role_guard.commit(HANDSHAKE_ROLE_ESTABLISHED);
                    return Ok(HandshakeStep::Done);
                }

                Err(Error::RemoteHandshakeApplyError(
                    "unexpected inbound handshake step on established dummy connection".to_string(),
                ))
            }
        }
    }

    async fn wait_for_channel_open(&self) -> Result<()> {
        if WAIT_FOR_DATA_CHANNEL_OPEN_PENDING.with(|pending| pending.get()) {
            std::future::pending::<()>().await;
        }
        if self.channel_is_open() {
            Ok(())
        } else {
            Err(Error::ChannelOpen(
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

    async fn new_connection(
        &self,
        cid: &str,
        callback: BoxedTransportCallback,
    ) -> Result<ConnectionRef<Self::Connection>> {
        self.pool.ensure_peer_slot_available(cid)?;

        let inner_callback = InnerTransportCallback::new(
            cid,
            callback,
            Notifier::default(),
            self.frame_pool.clone(),
        );
        let conn = DummyConnection::new(inner_callback, Arc::clone(&self.dispatched));

        let connection = self.pool.safely_insert(cid, conn).await?;
        let conn = connection.upgrade()?;
        if !conn.spawn_credit_pumps() {
            // Without its pumps the connection would advertise no credit after the first window.
            self.close_connection_if_current(&connection).await?;
            return Err(Error::CreditPumpUnavailable(cid.to_string()));
        }
        if CONTROLLED.with(std::cell::Cell::get) {
            let link = Arc::downgrade(conn.callback.link_credit());
            CONTROLLED_LINKS.with(|links| links.borrow_mut().push(link));
        }
        CONNS.insert(conn.rand_id.clone(), conn);

        Ok(connection)
    }

    async fn close_connection_if_current(
        &self,
        connection: &ConnectionRef<Self::Connection>,
    ) -> Result<bool> {
        self.pool.safely_remove_if_current(connection).await
    }

    async fn close_dyn_connection_if_current(
        &self,
        connection: &ConnectionRef<crate::core::transport::DynConnection>,
    ) -> Result<bool> {
        self.pool.safely_remove_if_current(connection).await
    }

    fn connection(&self, cid: &str) -> Result<ConnectionRef<Self::Connection>> {
        self.pool.connection(cid)
    }

    fn connection_ids(&self) -> Vec<String> {
        self.pool.connection_ids()
    }

    fn validate_inbound_handshake(&self, _step_index: u8, payload: &[u8]) -> Result<()> {
        let text = std::str::from_utf8(payload).map_err(|err| {
            Error::RemoteHandshakeApplyError(format!("invalid UTF-8 handshake payload: {err}"))
        })?;
        effective_max_message_size(text).map(|_| ())
    }
}
