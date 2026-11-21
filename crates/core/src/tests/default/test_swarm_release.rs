//! The release law (#1093): a swarm its owner dropped, with the runtime it ran on, is freed,
//! and its stores with it, whether or not it ever had a peer. The close law (#1098): a swarm
//! closed before it is dropped leaves none of its connections' sockets bound, and opens no
//! connection after the close.
//!
//! ```text
//!   build(store at d) ; connect(peer) ; drop(swarm) ; drop(runtime)
//!     ⊢ transport freed  ∧  open(d) = Ok
//!   build ; connect(peer) ; close(swarm) ; drop(swarm) ; drop(runtime)
//!     ⊢ ∀ a ∈ host candidates bound before the close: bind(a) = Ok
//! ```
//!
//! Each node runs on a runtime of its own, as two processes would, so dropping one node's
//! runtime ends that node's tasks and nothing of its peer's.

use std::any::Any;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use rings_transport::connection_pool::Pool;
use rings_transport::connection_ref::ConnectionRef;
use rings_transport::connections::DummyConnection;
use rings_transport::connections::DummyTransport;
use rings_transport::core::callback::BoxedTransportCallback;
use rings_transport::core::pool::ChannelLane;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::ConnectionStateSnapshot;
use rings_transport::core::transport::HandshakeStep;
use rings_transport::core::transport::LaneCreditView;
use rings_transport::core::transport::SendPermit;
use rings_transport::core::transport::TransportConnectionState;
use rings_transport::core::transport::TransportInterface;
use rings_transport::delivery::DeliveryFuture;
use rings_transport::delivery::SendCreditWait;

use super::fixture_builder;
use super::prepare_node;
use super::wait_for_connection_state;
use super::Node;
use super::TEST_TRAVERSAL_SERVERS;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::storage::file::test_root::TempRoot;
use crate::storage::file::FileStorage;
use crate::storage::RecordAuthority;

/// The record budget of the replay store under test, ample for one handshake's records.
const REPLAY_STORE_CAPACITY: u32 = 1024 * 1024;

/// A runtime of its own for one node, with the I/O and time drivers WebRTC needs.
fn node_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(Error::ServiceIOError)
}

/// Open the replay store at `root`, at the authority a node's replay store has.
async fn open_replay_store(root: &std::path::Path) -> Result<FileStorage> {
    FileStorage::new_with_cap_path_and_authority(
        REPLAY_STORE_CAPACITY,
        root,
        RecordAuthority::Authoritative,
    )
    .await
}

/// A fixture node whose replay store is the file store at `root`.
async fn prepare_node_on_replay_store(root: &std::path::Path) -> Result<Node> {
    let store = open_replay_store(root).await?;
    let builder = fixture_builder(&SecretKey::random(), TEST_TRAVERSAL_SERVERS)?;
    Ok(Node::build(builder.replay_storage(Box::new(store))))
}

/// One node and the runtime it runs on.
struct Running<'a> {
    node: &'a Node,
    runtime: &'a tokio::runtime::Runtime,
}

/// Connect `node` to `peer` by hand, each step on its own node's runtime, until both ends see
/// the connection `Connected`; returns `node`'s offer.
fn connect_by_hand(node: Running<'_>, peer: Running<'_>) -> Result<MessagePayload> {
    let offer = node
        .runtime
        .block_on(node.node.swarm.create_offer(peer.node.did()))?;
    let answer = peer
        .runtime
        .block_on(peer.node.swarm.answer_offer(offer.clone()))?
        .answer()
        .ok_or_else(|| Error::InvalidMessage("the peer counter-offered".to_string()))?;
    node.runtime
        .block_on(node.node.swarm.accept_answer(answer))?;
    node.runtime.block_on(wait_for_connection_state(
        node.node,
        peer.node.did(),
        TransportConnectionState::Connected,
    ))?;
    peer.runtime.block_on(wait_for_connection_state(
        peer.node,
        node.node.did(),
        TransportConnectionState::Connected,
    ))?;
    Ok(offer)
}

type LeakedSocketSlot = Arc<Mutex<Option<UdpSocket>>>;
type LeakedSocketList = Arc<Mutex<Vec<LeakedSocketSlot>>>;

/// A transport wrapper for `#1098` close-law verification without depending on `native-webrtc`:
/// each connection binds a real OS [`UdpSocket`] on `127.0.0.1:0` that stays retained in a
/// process-lifetime slot if the connection is merely dropped, and is unbound iff
/// [`ConnectionInterface::close`] runs.
struct SocketBoundConnection {
    inner: ConnectionRef<DummyConnection>,
    bound_addr: SocketAddr,
    socket: LeakedSocketSlot,
}

#[async_trait]
impl ConnectionInterface for SocketBoundConnection {
    type Error = rings_transport::error::Error;

    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn send_message_with_permit(
        &self,
        payload: Bytes,
        lane: ChannelLane,
        permit: SendPermit,
    ) -> rings_transport::error::Result<DeliveryFuture> {
        self.inner
            .send_message_with_permit(payload, lane, permit)
            .await
    }

    fn reserve_send_credit(&self, lane: ChannelLane) -> SendCreditWait<Self::Error> {
        self.inner.reserve_send_credit(lane)
    }

    fn send_credit_view(&self, lane: ChannelLane) -> LaneCreditView {
        self.inner.send_credit_view(lane)
    }

    fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
        self.inner.connection_state_snapshot()
    }

    fn max_message_size(&self) -> usize {
        self.inner.max_message_size()
    }

    async fn handshake_step(
        &self,
        inbound: Option<Bytes>,
    ) -> rings_transport::error::Result<HandshakeStep> {
        let is_initial = inbound.is_none();
        match self.inner.handshake_step(inbound).await? {
            HandshakeStep::Send(bytes) if is_initial => {
                let text = std::str::from_utf8(&bytes).unwrap_or_default();
                let with_cand = format!(
                    "{text}\r\na=candidate:1 1 udp 2130706431 {} {} typ host\r\n",
                    self.bound_addr.ip(),
                    self.bound_addr.port()
                );
                Ok(HandshakeStep::Send(Bytes::from(with_cand)))
            }
            other => Ok(other),
        }
    }

    async fn wait_for_channel_open(&self) -> rings_transport::error::Result<()> {
        self.inner.wait_for_channel_open().await
    }

    async fn close(&self) -> rings_transport::error::Result<()> {
        let _released = self
            .socket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.inner.close().await
    }
}

struct SocketBoundTransport {
    inner: DummyTransport,
    pool: Pool<SocketBoundConnection>,
    leaked_unless_closed: LeakedSocketList,
}

impl SocketBoundTransport {
    fn new(leaked_unless_closed: LeakedSocketList) -> Self {
        Self {
            inner: DummyTransport::new(TEST_TRAVERSAL_SERVERS, None, None),
            pool: Pool::new(),
            leaked_unless_closed,
        }
    }
}

#[async_trait]
impl TransportInterface for SocketBoundTransport {
    type Connection = SocketBoundConnection;
    type Error = rings_transport::error::Error;

    async fn new_connection(
        &self,
        cid: &str,
        callback: BoxedTransportCallback,
    ) -> rings_transport::error::Result<ConnectionRef<Self::Connection>> {
        self.pool.ensure_peer_slot_available(cid)?;
        let inner = self.inner.new_connection(cid, callback).await?;
        let udp = UdpSocket::bind("127.0.0.1:0").expect("OS UDP socket bind");
        let bound_addr = udp.local_addr().expect("OS UDP local_addr");
        let socket = Arc::new(Mutex::new(Some(udp)));
        self.leaked_unless_closed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::clone(&socket));
        self.pool
            .safely_insert(cid, SocketBoundConnection {
                inner,
                bound_addr,
                socket,
            })
            .await
    }

    async fn close_connection_if_current(
        &self,
        connection: &ConnectionRef<Self::Connection>,
    ) -> rings_transport::error::Result<bool> {
        let removed = self.pool.safely_remove_if_current(connection).await?;
        if let Ok(inner) = self.inner.connection(connection.cid()) {
            let _ = self.inner.close_connection_if_current(&inner).await;
        }
        Ok(removed)
    }

    fn connection(
        &self,
        cid: &str,
    ) -> rings_transport::error::Result<ConnectionRef<Self::Connection>> {
        self.pool.connection(cid)
    }

    fn connection_ids(&self) -> Vec<String> {
        self.pool.connection_ids()
    }

    fn validate_inbound_handshake(
        &self,
        step_index: u8,
        payload: &[u8],
    ) -> rings_transport::error::Result<()> {
        self.inner.validate_inbound_handshake(step_index, payload)
    }
}

fn bound_host_candidates(offer: &MessagePayload) -> Result<Vec<SocketAddr>> {
    let Message::ConnectNodeSend(send) = offer.transaction.data::<Message>()? else {
        return Err(Error::InvalidMessage(
            "the offer carries no handshake".to_string(),
        ));
    };
    let handshake = std::str::from_utf8(&send.handshake)
        .map_err(|err| Error::InvalidMessage(format!("non-utf8 handshake: {err}")))?;
    Ok(handshake
        .lines()
        .filter_map(host_candidate)
        .filter(|address| {
            matches!(UdpSocket::bind(address), Err(error) if error.kind() == ErrorKind::AddrInUse)
        })
        .collect())
}

/// The address of `line` if it is an SDP UDP host candidate:
/// `a=candidate:<foundation> <component> udp <priority> <address> <port> typ host …`.
fn host_candidate(line: &str) -> Option<SocketAddr> {
    let fields: Vec<&str> = line
        .strip_prefix("a=candidate:")?
        .split_whitespace()
        .collect();
    let [_, _, transport, _, address, port, "typ", "host", ..] = fields.as_slice() else {
        return None;
    };
    if !transport.eq_ignore_ascii_case("udp") {
        return None;
    }
    Some(SocketAddr::new(address.parse().ok()?, port.parse().ok()?))
}

/// A node that had a peer, dropped by its owner and then with its runtime, frees its
/// transport, and so its replay store: a store opened again on the same directory opens.
///
/// Both ends see the connection `Connected` before the drop, so the connection pool holds the
/// connection and its callback.
#[test]
fn test_a_dropped_node_that_had_a_peer_releases_its_replay_store() -> Result<()> {
    let root = TempRoot::new("swarm-release");
    let peer_runtime = node_runtime()?;
    let runtime = node_runtime()?;
    let peer = peer_runtime.block_on(prepare_node(SecretKey::random()));
    let node = runtime.block_on(prepare_node_on_replay_store(&root))?;
    connect_by_hand(
        Running {
            node: &node,
            runtime: &runtime,
        },
        Running {
            node: &peer,
            runtime: &peer_runtime,
        },
    )?;

    let transport = Arc::downgrade(&node.swarm.transport);
    runtime.block_on(async move { drop(node) });
    drop(runtime);

    assert!(
        transport.upgrade().is_none(),
        "the dropped node's transport is still alive"
    );
    peer_runtime.block_on(open_replay_store(&root))?;
    Ok(())
}

/// A node that had a peer, closed and then dropped with its runtime, leaves none of its
/// connection's sockets bound: every host candidate it offered, bound until then, binds
/// again. Between the close and the drop it refuses a new connection.
///
/// Without the close, the dropped node's peer connection is never closed, and its candidates
/// stay bound for the life of the process.
#[test]
fn test_a_closed_node_releases_its_connection_sockets() -> Result<()> {
    let retained = Arc::new(Mutex::new(Vec::new()));
    let peer_retained = Arc::new(Mutex::new(Vec::new()));
    let peer_runtime = node_runtime()?;
    let runtime = node_runtime()?;
    let build_socket_node = |list: LeakedSocketList| -> Result<Node> {
        let key = SecretKey::random();
        let session = crate::delegation::DelegateeKey::new_with_seckey(&key)?;
        let transport: rings_transport::core::transport::SharedTransport =
            Arc::new(SocketBoundTransport::new(list));
        let builder = crate::swarm::SwarmBuilder::new(
            crate::tests::TEST_NETWORK_ID,
            transport,
            Box::new(crate::storage::MemStorage::new()),
            session,
        )
        .dht_virtual_nodes(0)
        .replay_storage(crate::tests::volatile_replay_storage());
        Ok(Node::build(builder))
    };
    let peer = peer_runtime.block_on(async { build_socket_node(Arc::clone(&peer_retained)) })?;
    let node = runtime.block_on(async { build_socket_node(Arc::clone(&retained)) })?;
    let offer = connect_by_hand(
        Running {
            node: &node,
            runtime: &runtime,
        },
        Running {
            node: &peer,
            runtime: &peer_runtime,
        },
    )?;
    let candidates = bound_host_candidates(&offer)?;
    assert!(
        !candidates.is_empty(),
        "the offer names no bound host candidate"
    );

    runtime.block_on(node.swarm.close())?;
    assert!(matches!(
        runtime.block_on(node.swarm.create_offer(peer.did())),
        Err(Error::SwarmClosed)
    ));
    runtime.block_on(async move { drop(node) });
    drop(runtime);

    for address in candidates {
        UdpSocket::bind(address).map_err(Error::ServiceIOError)?;
    }
    drop(retained);
    Ok(())
}
