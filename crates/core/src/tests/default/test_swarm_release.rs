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

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::sync::Arc;

use rings_transport::core::transport::WebrtcConnectionState;

use super::fixture_builder;
use super::prepare_node;
use super::wait_for_connection_state;
use super::Node;
use super::TEST_ICE_SERVERS;
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
    let builder = fixture_builder(&SecretKey::random(), TEST_ICE_SERVERS)?;
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
        WebrtcConnectionState::Connected,
    ))?;
    peer.runtime.block_on(wait_for_connection_state(
        peer.node,
        node.node.did(),
        WebrtcConnectionState::Connected,
    ))?;
    Ok(offer)
}

/// The UDP host candidates (RFC 8839, section 5.1) a node named in its `offer` that are bound
/// now: those a fresh socket cannot bind. A candidate this host cannot bind for another reason
/// (an IPv6 link-local address without its scope) witnesses nothing and is left out.
fn bound_host_candidates(offer: &MessagePayload) -> Result<Vec<SocketAddr>> {
    let Message::ConnectNodeSend(send) = offer.transaction.data::<Message>()? else {
        return Err(Error::InvalidMessage(
            "the offer carries no SDP".to_string(),
        ));
    };
    // The handshake carries the SDP as a JSON string, as the answerer decodes it.
    let sdp: String = serde_json::from_str(&send.sdp).map_err(Error::Deserialize)?;
    Ok(sdp
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
    let peer_runtime = node_runtime()?;
    let runtime = node_runtime()?;
    let peer = peer_runtime.block_on(prepare_node(SecretKey::random()));
    let node = runtime.block_on(prepare_node(SecretKey::random()));
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
    Ok(())
}
