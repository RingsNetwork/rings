use std::cell::Cell;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
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
use crate::delivery::DeliveryFuture;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::IceServer;
use crate::notifier::Notifier;
use crate::pool::Pool;

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
    static DELIVERY: RefCell<VecDeque<(String, Event)>> = RefCell::new(VecDeque::new());
}

/// Test-only controlled delivery scheduler. When enabled (per thread), dummy
/// message/event delivery is queued instead of auto-dispatched, so a test can
/// drive the exact ordering and deterministically explore the timing-state space
/// (see `rings_core`'s `tests::default::dht_schedule`). Off by default; no effect
/// on normal runs.
pub mod controlled {
    use super::CONNS;
    use super::CONTROLLED;
    use super::DELIVERY;

    /// Turn the controlled scheduler on/off for the current thread. Turning it
    /// off clears this thread's queue.
    pub fn enable(on: bool) {
        CONTROLLED.with(|c| c.set(on));
        if !on {
            DELIVERY.with(|q| q.borrow_mut().clear());
        }
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
        let Some((rand_id, event)) = entry else {
            return false;
        };
        let Some(conn) = CONNS.get(&rand_id).map(|c| c.clone()) else {
            return false;
        };
        conn.handle_event(event).await;
        true
    }
}

enum Event {
    PeerConnectionStateChange(WebrtcConnectionState),
    DataChannelOpen,
    DataChannelClose,
    Message(Bytes),
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
            Event::PeerConnectionStateChange(state) => {
                self.callback.on_peer_connection_state_change(state).await
            }
            Event::DataChannelOpen => self.callback.on_data_channel_open().await,
            Event::DataChannelClose => self.callback.on_data_channel_close().await,
            Event::Message(data) => {
                if SEND_MESSAGE_DELAY {
                    random_delay().await;
                }
                self.callback.on_message(&data).await
            }
        }
    }

    fn remote_conn(&self) -> Option<Arc<DummyConnection>> {
        let Some(cid) = { self.remote_rand_id.lock().unwrap() }.clone() else {
            return None;
        };
        // The remote may already have been closed and removed from the global
        // map (e.g. during a disconnect). Return None instead of panicking, so
        // callers treat it like a closed connection.
        CONNS.get(&cid).map(|c| c.clone())
    }

    fn set_remote_rand_id(&self, rand_id: String) {
        let mut remote_rand_id = self.remote_rand_id.lock().unwrap();
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
            let mut webrtc_connection_state = self.webrtc_connection_state.lock().unwrap();

            if state == *webrtc_connection_state {
                return;
            }

            *webrtc_connection_state = state;
        }

        self.dispatch(Event::PeerConnectionStateChange(state));

        if state == WebrtcConnectionState::Connected {
            self.dispatch(Event::DataChannelOpen);
        }

        if matches!(
            state,
            WebrtcConnectionState::Closed | WebrtcConnectionState::Disconnected
        ) {
            self.dispatch(Event::DataChannelClose);
        }
    }
}

impl DummyTransport {
    /// Create a new [DummyTransport] instance.
    pub fn new(ice_servers: &str, _external_address: Option<String>) -> Self {
        let _ice_servers = IceServer::vec_from_str(ice_servers).unwrap();

        Self { pool: Pool::new() }
    }
}

#[async_trait]
impl ConnectionInterface for DummyConnection {
    type Sdp = String;
    type Error = Error;

    async fn send_message(&self, msg: TransportMessage) -> Result<DeliveryFuture> {
        self.webrtc_wait_for_data_channel_open().await?;

        let data = bincode::serialize(&msg).map(Bytes::from)?;
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
        *self.webrtc_connection_state.lock().unwrap()
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
        // Will pass if the state is connecting to prevent release connection in the `test_handshake_on_both_sides` test.
        // The connecting state means an offer is answered but not accepted by the other side.
        if matches!(
            self.webrtc_connection_state(),
            WebrtcConnectionState::Connected | WebrtcConnectionState::Connecting
        ) {
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
