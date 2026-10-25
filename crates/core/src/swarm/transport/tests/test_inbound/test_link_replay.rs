//! End-to-end replay windows by classification (#1081; durable-records, Law Window by
//! classification).
//!
//! Frames go through the real inbound pipeline of a node whose application declared delegated
//! replay for [`LINK_PREFIX`]. Each case reads the durable replay store, shared with the test,
//! and the replay counters:
//!
//! - link-local (a declaring namespace, from the authenticated neighbour that originated it):
//!   no record is written, and a second delivery on the same link is refused as a replay;
//! - the peer's next link generation, once the first retired, opens an empty window;
//! - a relayed copy, and a namespace that does not declare, keep the durable window.

use std::sync::Arc;

use super::super::pending_peer_with_key;
use super::super::relayed_payload;
use super::super::PendingPeer;
use super::local_wire;
use crate::chunk::Chunk;
use crate::delegation::DelegateeKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::ReplayRecord;
use crate::message::ReplayWindow;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;
use crate::tests::fixed_secret_keys;
use crate::tests::TEST_NETWORK_ID;

/// Payload prefix of the namespace the test application declares delegated replay for.
const LINK_PREFIX: &[u8] = b"linked/";

/// A local swarm whose application delegated the replay of [`LINK_PREFIX`], over a durable
/// replay store the test reads.
struct Harness {
    /// The local swarm.
    swarm: Swarm,
    /// Its transport.
    transport: Arc<SwarmTransport>,
    /// Its replay store.
    replay: Arc<MemStorage<ReplayRecord>>,
}

impl Harness {
    /// A fresh local swarm owned by `key`.
    fn new(key: &crate::ecc::SecretKey) -> Result<Self> {
        let local = DelegateeKey::new_with_seckey(key)?;
        let replay = Arc::new(MemStorage::new());
        let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
            .replay_storage(Box::new(Arc::clone(&replay)))
            .build();
        swarm.set_replay_delegation(LINK_PREFIX.to_vec(), ReplayWindow::Link);
        let transport = Arc::clone(&swarm.transport);
        Ok(Self {
            swarm,
            transport,
            replay,
        })
    }

    /// A fresh authenticated connection generation of the peer behind `key`.
    async fn link(&self, key: &crate::ecc::SecretKey) -> Result<PendingPeer> {
        let peer = pending_peer_with_key(&self.transport, key.clone()).await?;
        peer.activate(&self.transport)?;
        Ok(peer)
    }

    /// Deliver `frame` over `carrier`'s connection.
    async fn deliver(&self, carrier: &PendingPeer, frame: &bytes::Bytes) -> Result<()> {
        carrier
            .callback
            .on_admitted_message_for_test(&carrier.peer.to_string(), frame)
            .await
            .map_err(|error| match error.downcast::<Error>() {
                Ok(error) => *error,
                Err(error) => Error::InvalidMessage(error.to_string()),
            })
    }

    /// The durable replay records written.
    async fn records(&self) -> Result<u32> {
        self.replay.count().await
    }

    /// Transactions refused as replays.
    fn replays(&self) -> u64 {
        self.transport.replay_counters().replay
    }
}

/// A frame of a custom message of `payload` for `local`, whose transaction `origin` signed at
/// `sequence` and whose hop `carrier` signed.
fn wire(
    payload: &[u8],
    origin: &DelegateeKey,
    carrier: &DelegateeKey,
    local: crate::dht::Did,
    sequence: u64,
) -> Result<bytes::Bytes> {
    relayed_payload(Message::custom(payload)?, origin, carrier, local, sequence)?.to_wire()
}

/// Link-local: no durable record is written, and the same transaction delivered again on the
/// same link is refused by the link's window.
#[tokio::test]
async fn test_a_link_local_transaction_is_admitted_on_its_link_alone() -> Result<()> {
    let [local, neighbour] = fixed_secret_keys::<2>()?;
    let harness = Harness::new(&local)?;
    let neighbour = harness.link(&neighbour).await?;
    let frame = wire(
        b"linked/cell",
        &neighbour.session,
        &neighbour.session,
        harness.swarm.did(),
        0,
    )?;
    harness.deliver(&neighbour, &frame).await?;
    assert_eq!(harness.records().await?, 0);
    assert_eq!(harness.replays(), 0);

    assert!(matches!(
        harness.deliver(&neighbour, &frame).await,
        Err(Error::TransactionReplay { .. })
    ));
    assert_eq!(harness.replays(), 1);
    assert_eq!(harness.records().await?, 0);
    Ok(())
}

/// The window lives one link generation: the peer's next generation opens an empty window, so
/// the transaction is admitted there again, and its namespace's own replay key (Obligation R) is
/// what refuses it then; still nothing is written.
#[tokio::test]
async fn test_a_new_link_generation_opens_an_empty_window() -> Result<()> {
    let [local, neighbour] = fixed_secret_keys::<2>()?;
    let harness = Harness::new(&local)?;
    let first = harness.link(&neighbour).await?;
    let frame = wire(
        b"linked/cell",
        &first.session,
        &first.session,
        harness.swarm.did(),
        0,
    )?;
    harness.deliver(&first, &frame).await?;
    harness.transport.disconnect(first.peer).await?;

    let next = harness.link(&neighbour).await?;
    harness.deliver(&next, &frame).await?;
    assert_eq!(harness.replays(), 0);
    assert_eq!(harness.records().await?, 0);
    Ok(())
}

/// A relayed copy is never link-local: its carrier is not its origin, so it meets the durable
/// window, which writes its record and refuses the copy delivered again.
#[tokio::test]
async fn test_a_relayed_copy_meets_the_durable_window() -> Result<()> {
    let [local, origin, carrier] = fixed_secret_keys::<3>()?;
    let harness = Harness::new(&local)?;
    let carrier = harness.link(&carrier).await?;
    let origin = DelegateeKey::new_with_seckey(&origin)?;
    let frame = wire(
        b"linked/cell",
        &origin,
        &carrier.session,
        harness.swarm.did(),
        0,
    )?;
    harness.deliver(&carrier, &frame).await?;
    assert_eq!(harness.records().await?, 1);

    assert!(matches!(
        harness.deliver(&carrier, &frame).await,
        Err(Error::TransactionReplay { .. })
    ));
    assert_eq!(harness.replays(), 1);
    Ok(())
}

/// A namespace that does not declare delegated replay keeps the durable window, from the
/// authenticated neighbour too.
#[tokio::test]
async fn test_another_namespace_keeps_the_durable_window() -> Result<()> {
    let [local, neighbour] = fixed_secret_keys::<2>()?;
    let harness = Harness::new(&local)?;
    let neighbour = harness.link(&neighbour).await?;
    let frame = wire(
        b"other/message",
        &neighbour.session,
        &neighbour.session,
        harness.swarm.did(),
        0,
    )?;
    harness.deliver(&neighbour, &frame).await?;
    assert_eq!(harness.records().await?, 1);
    Ok(())
}

/// A link-local transaction reassembled from chunks, as an onion cell is, is classified as its
/// original: admitted on its link, with nothing written.
#[tokio::test]
async fn test_a_reassembled_link_local_transaction_writes_nothing() -> Result<()> {
    let [local, neighbour] = fixed_secret_keys::<2>()?;
    let harness = Harness::new(&local)?;
    let neighbour = harness.link(&neighbour).await?;
    let local = harness.swarm.did();
    let original =
        neighbour.custom_payload(&harness.transport, &[LINK_PREFIX, &[7; 512]].concat())?;
    let chunks: Vec<Chunk> = Chunk::stream(original.to_wire()?, 64).collect();
    assert!(chunks.len() > 1);
    for chunk in chunks {
        let frame = local_wire(Message::Chunk(chunk), &neighbour.session, local)?;
        harness.deliver(&neighbour, &frame).await?;
    }
    assert_eq!(harness.records().await?, 0);
    assert_eq!(harness.replays(), 0);
    Ok(())
}
