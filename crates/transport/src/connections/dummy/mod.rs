use std::cell::Cell;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use lazy_static::lazy_static;
use rand::distributions::Distribution;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::callback::InnerTransportCallback;
use crate::connection_ref::ConnectionRef;
use crate::core::callback::BoxedTransportCallback;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::TransportInterface;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;
use crate::delivery::DeliveryFuture;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::parse_ice_servers_or_warn;
use crate::notifier::Notifier;
use crate::pool::Pool;
use crate::sync_utils::lock_recover;
use crate::webrtc_config::WebrtcUdpPortRange;

/// Max delay in ms on sending message
const DUMMY_DELAY_MAX: u64 = 100;
/// Min delay in ms on sending message
const DUMMY_DELAY_MIN: u64 = 10;
/// Config random delay when send message
const SEND_MESSAGE_DELAY: bool = true;

lazy_static! {
    static ref CONNS: DashMap<String, Arc<DummyConnection>> = DashMap::new();
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
    /// Test-only controlled delivery queue: `(target connection rand_id, event)`,
    /// populated instead of auto-dispatching while `CONTROLLED` is on.
    static DELIVERY: RefCell<VecDeque<(String, Event)>> = const { RefCell::new(VecDeque::new()) };
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
    /// Test-only per-thread threshold that makes `send_message` stay pending after
    /// this many messages have already been dispatched.
    static SEND_MESSAGE_PENDING_AFTER_SENT_COUNT: Cell<Option<usize>> = const { Cell::new(None) };
    /// Test-only per-thread switch that makes `send_message` report local success
    /// without dispatching the message to the remote callback.
    static DROP_MESSAGES: Cell<bool> = const { Cell::new(false) };
}

/// Test-only controlled delivery scheduler. When enabled (per thread), dummy
/// message/event delivery is queued instead of auto-dispatched, so a test can
/// drive the exact ordering and deterministically explore the timing-state space
/// (see `rings_core`'s `tests::default::test_dht_schedule`). Off by default; no effect
/// on normal runs.
pub mod controlled {
    use super::CONNS;
    use super::CONTROLLED;
    use super::DELIVERY;
    use super::DROP_MESSAGES;
    use super::MAX_MESSAGE_SIZE;
    use super::NEXT_CALLBACK_CID;
    use super::SEND_MESSAGE_PENDING;
    use super::SEND_MESSAGE_PENDING_AFTER_SENT_COUNT;
    use super::WAIT_FOR_DATA_CHANNEL_OPEN_PENDING;

    /// Turn the controlled scheduler on/off for the current thread. Turning it
    /// off clears this thread's queue.
    pub fn enable(on: bool) {
        CONTROLLED.with(|c| c.set(on));
        if !on {
            DELIVERY.with(|q| q.borrow_mut().clear());
            NEXT_CALLBACK_CID.with(|next| {
                *next.borrow_mut() = None;
            });
            WAIT_FOR_DATA_CHANNEL_OPEN_PENDING.with(|pending| pending.set(false));
            SEND_MESSAGE_PENDING.with(|pending| pending.set(false));
            SEND_MESSAGE_PENDING_AFTER_SENT_COUNT.with(|threshold| threshold.set(None));
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

    /// Test hook: force `send_message` to stay pending once this thread has already dispatched
    /// `threshold` messages. `None` disables the hook.
    pub fn set_send_message_pending_after_sent_count(threshold: Option<usize>) {
        SEND_MESSAGE_PENDING_AFTER_SENT_COUNT.with(|pending_after| pending_after.set(threshold));
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
        DELIVERY.with(|q| q.borrow().len())
    }

    /// Deliver the queued event at `index` to its target connection — invoking
    /// the real handler, which may enqueue further events. Returns false if the
    /// index is out of range or the target connection is gone.
    pub async fn deliver(index: usize) -> bool {
        let entry = DELIVERY.with(|q| q.borrow_mut().remove(index));
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
        let index = DELIVERY.with(|q| {
            q.borrow()
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
    Message(Bytes),
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
pub struct DummyConnection {
    rand_id: String,
    callback: InnerTransportCallback,
    event_sender: mpsc::UnboundedSender<Event>,
    remote_rand_id: Arc<Mutex<Option<String>>>,
    event_listener: JoinHandle<()>,
    webrtc_connection_state: Arc<Mutex<WebrtcConnectionState>>,
}

/// [DummyTransport] manages all the [DummyConnection] and
/// provides methods to create, get and close connections.
pub struct DummyTransport {
    pool: Pool<DummyConnection>,
}

impl DummyConnection {
    fn new(callback: InnerTransportCallback) -> Self {
        let rand_id = random(0, 10000000000).to_string();

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
            callback,
            event_sender: tx,
            remote_rand_id: Default::default(),
            event_listener,
            webrtc_connection_state: Arc::new(Mutex::new(WebrtcConnectionState::New)),
        }
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
            Event::Message(data) => {
                if SEND_MESSAGE_DELAY && !CONTROLLED.with(|c| c.get()) {
                    random_delay().await;
                }
                self.callback.on_message(&data).await
            }
        }
    }

    fn remote_rand_id(&self) -> MutexGuard<'_, Option<String>> {
        lock_recover(&self.remote_rand_id)
    }

    fn connection_state(&self) -> MutexGuard<'_, WebrtcConnectionState> {
        lock_recover(&self.webrtc_connection_state)
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
        if CONTROLLED.with(|c| c.get()) {
            DELIVERY.with(|q| q.borrow_mut().push_back((self.rand_id.clone(), event)));
            true
        } else {
            self.event_sender.send(event).is_ok()
        }
    }

    async fn set_webrtc_connection_state(&self, state: WebrtcConnectionState) {
        {
            let mut webrtc_connection_state = self.connection_state();

            if state == *webrtc_connection_state {
                return;
            }

            *webrtc_connection_state = state;
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
}

impl DummyTransport {
    /// Create a new [DummyTransport] instance.
    pub fn new(
        ice_servers: &str,
        _external_address: Option<String>,
        _udp_port_range: Option<WebrtcUdpPortRange>,
    ) -> Self {
        let _ = parse_ice_servers_or_warn(ice_servers, "dummy");

        Self { pool: Pool::new() }
    }
}

#[async_trait]
impl ConnectionInterface for DummyConnection {
    type Sdp = String;
    type Error = Error;

    async fn send_message(&self, msg: TransportMessage) -> Result<DeliveryFuture> {
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
        SENT_COUNT.with(|c| c.set(c.get() + 1));

        let data = bincode::serialize(&msg).map(Bytes::from)?;
        if DROP_MESSAGES.with(|drop| drop.get()) {
            return Ok(Box::pin(async { Ok(()) }));
        }
        // The remote connection may have been torn down between the data
        // channel check and here (the dummy analogue of sending on a channel
        // that just closed). Mimic a real transport: fail gracefully instead of
        // panicking.
        let remote = self.remote_conn().ok_or_else(|| {
            Error::MessageNotDelivered("dummy remote connection is gone".to_string())
        })?;
        if !remote.dispatch(Event::Message(data)) {
            return Err(Error::MessageNotDelivered(
                "dummy remote connection is closed".to_string(),
            ));
        }

        // The dummy backend delivers synchronously in-memory, so delivery is
        // immediately complete.
        Ok(Box::pin(async { Ok(()) }))
    }

    fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        *self.connection_state()
    }

    fn data_channel_is_open(&self) -> Result<bool> {
        // Dummy handshakes use `Connecting` for the interval after an offer is
        // answered but before the opposite side has accepted the answer. Tests
        // can deliver a data-channel-open callback in that interval to model
        // browser timing where SCTP opens before ICE reaches `Connected`.
        Ok(matches!(
            self.webrtc_connection_state(),
            WebrtcConnectionState::Connected | WebrtcConnectionState::Connecting
        ))
    }

    fn max_message_size(&self) -> usize {
        match MAX_MESSAGE_SIZE.with(|m| m.get()) {
            0 => MAX_DATA_CHANNEL_MESSAGE_SIZE,
            n => n,
        }
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
        CONNS.remove(&self.rand_id);
        self.event_listener.abort();

        self.set_webrtc_connection_state(WebrtcConnectionState::Closed)
            .await;

        // simulate remote closing if it's not closed
        if let Some(remote_conn) = self.remote_conn() {
            if remote_conn.webrtc_connection_state() != WebrtcConnectionState::Closed {
                remote_conn
                    .set_webrtc_connection_state(WebrtcConnectionState::Disconnected)
                    .await;
                remote_conn
                    .set_webrtc_connection_state(WebrtcConnectionState::Closed)
                    .await;
            }
        }

        Ok(())
    }
}

#[async_trait]
impl TransportInterface for DummyTransport {
    type Connection = DummyConnection;
    type Error = Error;

    async fn new_connection(&self, cid: &str, callback: BoxedTransportCallback) -> Result<()> {
        if let Ok(existed_conn) = self.pool.connection(cid) {
            if matches!(
                existed_conn.webrtc_connection_state(),
                WebrtcConnectionState::New
                    | WebrtcConnectionState::Connecting
                    | WebrtcConnectionState::Connected
            ) {
                return Err(Error::ConnectionAlreadyExists(cid.to_string()));
            }
        }

        let inner_callback = InnerTransportCallback::new(cid, callback, Notifier::default());
        let conn = DummyConnection::new(inner_callback);

        self.pool.safely_insert(cid, conn)?;

        let conn = self.connection(cid)?.upgrade()?;
        CONNS.insert(conn.rand_id.clone(), conn);

        Ok(())
    }

    async fn close_connection(&self, cid: &str) -> Result<()> {
        self.pool.safely_remove(cid).await
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

async fn random_delay() {
    tokio::time::sleep(Duration::from_millis(random(
        DUMMY_DELAY_MIN,
        DUMMY_DELAY_MAX,
    )))
    .await;
}

fn random(low: u64, high: u64) -> u64 {
    let range = rand::distributions::Uniform::new(low, high);
    let mut rng = rand::thread_rng();
    range.sample(&mut rng)
}
