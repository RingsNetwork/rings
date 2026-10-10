use std::sync::atomic::AtomicBool;
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
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::callback::InnerTransportCallback;
use crate::callback::NodeReceiveLoad;
use crate::connection_ref::ConnectionRef;
use crate::core::callback::BoxedTransportCallback;
use crate::core::credit::CreditIndex;
use crate::core::pool::ChannelLane;
use crate::core::transport::stored_max_message_size;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateSnapshot;
use crate::core::transport::IrrevocableSendGuard;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportInterface;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::delivery::DeliveryFuture;
use crate::delivery::SendCreditWait;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::parse_ice_servers_or_warn;
use crate::notifier::Notifier;
use crate::pool::Pool;
use crate::sync_utils::lock_recover;
use crate::webrtc_config::WebrtcUdpPortRange;

mod delay;
mod event;
mod retirement;
mod state;

use self::delay::random;
use self::delay::random_delay;
use self::event::Event;
use self::retirement::DummyRetirementFence;
use self::state::ControlledDeliveryEntry;
use self::state::ACTIVE_DELIVERY_GATE;
use self::state::CLOSE_PENDING;
use self::state::CONTROLLED;
use self::state::CONTROLLED_RNG_STATE;
use self::state::CONTROLLED_VIRTUAL_MS;
use self::state::DELIVERY;
use self::state::DELIVERY_ENQUEUED;
use self::state::DELIVERY_FUTURE_PENDING;
use self::state::DROP_MESSAGES;
use self::state::HELD_DELIVERY_GATE;
use self::state::IRREVOCABLE_SEND_GATE;
use self::state::IRREVOCABLE_SEND_GATE_WAITING;
use self::state::MAX_MESSAGE_SIZE;
use self::state::NEXT_CALLBACK_CID;
use self::state::NEXT_DELIVERY_GATE;
use self::state::POST_PERMIT_SEND_GATE;
use self::state::POST_PERMIT_SEND_GATE_WAITING;
use self::state::SEND_MESSAGE_GATE;
use self::state::SEND_MESSAGE_GATE_WAITING;
use self::state::SEND_MESSAGE_PENDING;
use self::state::SEND_MESSAGE_PENDING_AFTER_SENT_COUNT;
use self::state::SENT_COUNT;
use self::state::WAIT_FOR_DATA_CHANNEL_OPEN_PENDING;
use self::state::WITHHELD_CREDIT;

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
    webrtc: WebrtcConnectionState,
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
            None => matches!(self.webrtc, WebrtcConnectionState::Connected),
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
    /// The frames this node lends its connections' lanes, shared by every connection.
    receive_load: NodeReceiveLoad,
}

impl DummyConnection {
    pub(crate) fn generation_id(&self) -> &str {
        &self.rand_id
    }

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

    /// Start this connection's credit pumps, one per lane: every credit its receive side
    /// advertises is granted to the paired connection directly, the dummy link carrying credit
    /// without latency. A credit is released only for a frame that arrived, so it always has a
    /// paired connection to go to; once that connection is gone its senders are gone with it,
    /// and the credit has no one to reach. The pumps stop when this connection's callback is
    /// dropped. Post: `false` when a pump cannot be spawned.
    fn spawn_credit_pumps(self: &Arc<Self>) -> bool {
        CreditIndex::ALL.into_iter().all(|index| {
            let link = Arc::clone(self.callback.link_credit());
            let connection = Arc::downgrade(self);
            let pump = async move {
                while let Some(credit) = link.next_credit(index).await {
                    let Some(conn) = connection.upgrade() else {
                        continue;
                    };
                    if WITHHELD_CREDIT.with(|withheld| withheld.borrow().contains(&conn.rand_id)) {
                        continue;
                    }
                    if let Some(remote) = conn.remote_conn() {
                        remote.callback.link_credit().grant(index.lane(), credit);
                    }
                }
            };
            rings_runtime::spawn_detached(pump).is_ok()
        })
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
                if SEND_MESSAGE_DELAY {
                    random_delay().await;
                }
                // Arrival never waits on the protocol: the frame joins its lane's FIFO, as a
                // native or browser channel's frames do (see `InnerTransportCallback`).
                self.callback.dispatch_admitted_frame(frame, lane);
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
            receive_load: NodeReceiveLoad::new(),
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
    lane: ChannelLane,
    target: DummySendTarget,
    permit: IrrevocableSendGuard<F>,
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
        SENT_COUNT.with(|count| count.set(count.get() + 1));
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

    /// The dummy link delivers one connection's messages over one ordered event queue, which
    /// meets the lane law for every lane at once (a controlled test may reorder events on
    /// purpose), so the lane needs no channel of its own here.
    async fn send_message_with_permit(
        &self,
        msg: TransportMessage,
        lane: ChannelLane,
        mut permit: SendPermit,
    ) -> Result<DeliveryFuture> {
        self.webrtc_wait_for_data_channel_open().await?;
        let _credit = self
            .callback
            .credit_for_send(&msg, lane, &mut permit)
            .await?;
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
                let result = complete_irrevocable_send(
                    &connection_state,
                    data,
                    lane,
                    target,
                    permit_retirement,
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
        )
    }

    fn reserve_send_credit(&self, lane: ChannelLane) -> SendCreditWait<Error> {
        Box::pin(self.callback.reserve_credit(lane))
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
            self.receive_load.clone(),
        );
        let conn = DummyConnection::new(inner_callback);

        let connection = self.pool.safely_insert(cid, conn).await?;
        let conn = connection.upgrade()?;
        if !conn.spawn_credit_pumps() {
            // Without its pumps the connection would advertise no credit after the first window.
            self.close_connection_if_current(&connection).await?;
            return Err(Error::CreditPumpUnavailable(cid.to_string()));
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

    fn connection(&self, cid: &str) -> Result<ConnectionRef<Self::Connection>> {
        self.pool.connection(cid)
    }

    fn connection_ids(&self) -> Vec<String> {
        self.pool.connection_ids()
    }
}
