//! The release law (#1093): a swarm its owner dropped, with the runtime it ran on, is freed,
//! and its stores with it, whether or not it ever had a peer. (A peer connection it never
//! closed still keeps its sockets bound, #1098.)
//!
//! ```text
//!   build(store at d) ; connect(peer) ; drop(swarm) ; drop(runtime)
//!     ⊢ transport freed  ∧  open(d) = Ok
//! ```
//!
//! Each node runs on a runtime of its own, as two processes would, so dropping one node's
//! runtime ends that node's tasks and nothing of its peer's.

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

/// A node that had a peer, dropped by its owner and then with its runtime, frees its
/// transport, and so its replay store: a store opened again on the same directory opens.
///
/// The connection is made by hand, each step on its own node's runtime, and both ends see it
/// `Connected` before the drop, so the connection pool holds the connection and its callback.
#[test]
fn test_a_dropped_node_that_had_a_peer_releases_its_replay_store() -> Result<()> {
    let root = TempRoot::new("swarm-release");
    let peer_runtime = node_runtime()?;
    let runtime = node_runtime()?;
    let peer = peer_runtime.block_on(prepare_node(SecretKey::random()));
    let node = runtime.block_on(prepare_node_on_replay_store(&root))?;

    let offer = runtime.block_on(node.swarm.create_offer(peer.did()))?;
    let answer = peer_runtime
        .block_on(peer.swarm.answer_offer(offer))?
        .answer()
        .ok_or_else(|| Error::InvalidMessage("the peer counter-offered".to_string()))?;
    runtime.block_on(node.swarm.accept_answer(answer))?;
    runtime.block_on(wait_for_connection_state(
        &node,
        peer.did(),
        WebrtcConnectionState::Connected,
    ))?;
    peer_runtime.block_on(wait_for_connection_state(
        &peer,
        node.did(),
        WebrtcConnectionState::Connected,
    ))?;

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
