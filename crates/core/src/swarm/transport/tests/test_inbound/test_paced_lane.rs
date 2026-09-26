//! End-to-end lane selection of the paced direct-edge lane (#888).
//!
//! Frames go through the real inbound pipeline, and each case asserts which lane the origin's
//! quota record is keyed by. That depends on no clock; the rate bounds themselves are checked
//! on a simulated clock in the quota module.

use std::num::NonZeroU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;

use super::local_wire;
use crate::chunk::Chunk;
use crate::delegation::DelegateeKey;
use crate::dht::delivery::NextHop;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::HopBudget;
use crate::message::Message;
use crate::message::MessageCategory;
use crate::message::MessagePayload;
use crate::message::MessageRelay;
use crate::message::MessageSigner;
use crate::message::OriginQuotaLaneId;
use crate::message::PacedLane;
use crate::message::PacedLaneId;
use crate::message::PacedRate;
use crate::message::Transaction;
use crate::storage::MemStorage;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;
use crate::tests::TEST_NETWORK_ID;

/// Payload prefix the test application registers a paced lane for.
const PACED_PREFIX: &[u8] = b"paced/";

/// Identity of the one lane the test application registers.
const PACED_LANE: PacedLaneId = PacedLaneId::new(1);

/// The record lane of paced traffic.
const PACED: OriginQuotaLaneId = OriginQuotaLaneId::Paced(PACED_LANE);

/// The record lane of default Application traffic.
const APPLICATION: OriginQuotaLaneId = OriginQuotaLaneId::Class(MessageCategory::Application);

/// A stand-in for the application registry: payloads under [`PACED_PREFIX`] belong to one
/// paced lane, every other payload to none.
#[derive(Default)]
struct PacedRegistryCallback {
    /// How often core consulted the registry.
    consulted: AtomicUsize,
}

#[async_trait]
impl SwarmCallback for PacedRegistryCallback {
    fn paced_lane(&self, application_payload: &[u8]) -> Option<PacedLane> {
        self.consulted.fetch_add(1, Ordering::SeqCst);
        let rate = PacedRate::new(NonZeroU64::new(16_384)?, NonZeroU64::new(150)?);
        application_payload
            .starts_with(PACED_PREFIX)
            .then_some(PacedLane::new(PACED_LANE, rate))
    }
}

/// One connected peer: its account, its session key, and the callback of its connection.
struct Peer {
    /// Account DID, the connection's peer identity.
    did: Did,
    /// Session key signing on the peer's behalf.
    session: DelegateeKey,
    /// Inbound callback of the connection to this peer.
    callback: InnerSwarmCallback,
}

/// A local swarm with the test application installed.
struct Harness {
    /// The local swarm.
    swarm: Swarm,
    /// Its transport.
    transport: Arc<SwarmTransport>,
    /// The test application registry.
    app: Arc<PacedRegistryCallback>,
}

impl Harness {
    /// A fresh local swarm.
    fn new() -> Result<Self> {
        let local = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        let app = Arc::new(PacedRegistryCallback::default());
        let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
            .callback(app.clone())
            .build();
        let transport = Arc::clone(&swarm.transport);
        Ok(Self {
            swarm,
            transport,
            app,
        })
    }

    /// A peer whose connection is authenticated when `authenticated` holds.
    async fn peer(&self, authenticated: bool) -> Result<Peer> {
        let key = SecretKey::random();
        let did: Did = key.address().into();
        let session = DelegateeKey::new_with_seckey(&key)?;
        let callback = if authenticated {
            let offer = InnerSwarmCallback::new(Arc::clone(&self.transport), self.app.clone());
            let (attempt, _offer) = self
                .transport
                .prepare_connection_offer_with_attempt(did, offer)
                .await?;
            assert!(self.transport.activate_connection_for_test(attempt)?);
            InnerSwarmCallback::new(Arc::clone(&self.transport), self.app.clone())
                .with_pending_connection_attempt(attempt)
        } else {
            InnerSwarmCallback::new(Arc::clone(&self.transport), self.app.clone())
        };
        Ok(Peer {
            did,
            session,
            callback,
        })
    }

    /// Deliver `frame` over `carrier`'s connection; a rejection is not an error here.
    async fn deliver(&self, carrier: &Peer, frame: &bytes::Bytes) {
        let _admitted = carrier
            .callback
            .on_admitted_message_for_test(&carrier.did.to_string(), frame)
            .await;
    }

    /// The lanes holding a quota record of `origin`.
    async fn lanes(&self, origin: Did) -> Vec<OriginQuotaLaneId> {
        self.transport.origin_quota_lanes_for_test(origin).await
    }
}

/// A frame whose transaction `origin` signed and whose hop `carrier` signed, for `local`.
fn wire(
    payload: &[u8],
    origin: &DelegateeKey,
    carrier: &DelegateeKey,
    local: Did,
) -> Result<bytes::Bytes> {
    let transaction = Transaction::new(
        local,
        uuid::Uuid::new_v4(),
        0,
        None,
        Message::custom(payload)?,
        MessageSigner::new(origin, TEST_NETWORK_ID),
    )?;
    MessagePayload::new(
        transaction,
        MessageSigner::new(carrier, TEST_NETWORK_ID),
        MessageRelay::new(NextHop::toward(local), local, HopBudget::MAX),
    )?
    .to_wire()
}

#[tokio::test]
async fn test_neighbours_own_registered_traffic_takes_the_paced_lane() -> Result<()> {
    let harness = Harness::new()?;
    let neighbour = harness.peer(true).await?;
    let frame = wire(
        b"paced/cell",
        &neighbour.session,
        &neighbour.session,
        harness.swarm.did(),
    )?;
    harness.deliver(&neighbour, &frame).await;
    assert_eq!(harness.lanes(neighbour.did).await, vec![PACED]);
    Ok(())
}

#[tokio::test]
async fn test_neighbours_unregistered_namespace_keeps_the_application_lane() -> Result<()> {
    let harness = Harness::new()?;
    let neighbour = harness.peer(true).await?;
    let frame = wire(
        b"other/message",
        &neighbour.session,
        &neighbour.session,
        harness.swarm.did(),
    )?;
    harness.deliver(&neighbour, &frame).await;
    assert_eq!(harness.lanes(neighbour.did).await, vec![APPLICATION]);
    Ok(())
}

#[tokio::test]
async fn test_relayed_origin_cannot_claim_the_paced_lane() -> Result<()> {
    let harness = Harness::new()?;
    let neighbour = harness.peer(true).await?;
    let relayed = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let frame = wire(
        b"paced/cell",
        &relayed,
        &neighbour.session,
        harness.swarm.did(),
    )?;
    harness.deliver(&neighbour, &frame).await;
    assert_eq!(harness.lanes(relayed.delegator_did()).await, vec![
        APPLICATION
    ]);
    // Ineligible traffic never reaches the application registry.
    assert_eq!(harness.app.consulted.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn test_neighbours_origin_over_another_neighbour_keeps_the_application_lane() -> Result<()> {
    let harness = Harness::new()?;
    let origin = harness.peer(true).await?;
    let carrier = harness.peer(true).await?;
    let frame = wire(
        b"paced/cell",
        &origin.session,
        &carrier.session,
        harness.swarm.did(),
    )?;
    harness.deliver(&carrier, &frame).await;
    assert_eq!(harness.lanes(origin.did).await, vec![APPLICATION]);
    Ok(())
}

#[tokio::test]
async fn test_unauthenticated_peer_claiming_its_origin_never_takes_the_paced_lane() -> Result<()> {
    let harness = Harness::new()?;
    let stranger = harness.peer(false).await?;
    let frame = wire(
        b"paced/cell",
        &stranger.session,
        &stranger.session,
        harness.swarm.did(),
    )?;
    harness.deliver(&stranger, &frame).await;
    // Admitted, so the lane choice is observed, and charged to the default lane.
    assert_eq!(harness.lanes(stranger.did).await, vec![APPLICATION]);
    assert_eq!(harness.app.consulted.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn test_reassembled_neighbour_traffic_takes_the_paced_lane() -> Result<()> {
    let harness = Harness::new()?;
    let neighbour = harness.peer(true).await?;
    let local = harness.swarm.did();
    let original = MessagePayload::new_send(
        Message::custom(&[PACED_PREFIX, &[7; 512]].concat())?,
        MessageSigner::new(&neighbour.session, TEST_NETWORK_ID),
        local,
        local,
    )?;
    let chunks: Vec<Chunk> = Chunk::stream(original.to_wire()?, 64).collect();
    assert!(chunks.len() > 1);
    for chunk in chunks {
        let frame = local_wire(Message::Chunk(chunk), &neighbour.session, local)?;
        neighbour
            .callback
            .on_admitted_message_for_test(&neighbour.did.to_string(), &frame)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    }
    assert_eq!(harness.lanes(neighbour.did).await, vec![PACED]);
    Ok(())
}
