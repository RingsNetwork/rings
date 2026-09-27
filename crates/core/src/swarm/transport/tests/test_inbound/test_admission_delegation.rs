//! End-to-end delegated admission (#888).
//!
//! Frames go through the real inbound pipeline, and each case reads the origin's Application
//! quota record after one admission. A stored record holds its tokens as of that admission, so
//! the assertions are exact and depend on no clock:
//!
//! - message limit skipped: the message bucket is still full (`burst`), and the byte bucket holds
//!   `byte_burst − max(cost, DELEGATED_MIN_CHARGE)`;
//! - message limit enforced: the buckets hold `burst − 1` and `byte_burst − cost`.

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
use crate::message::OriginQuotaConfig;
use crate::message::OriginQuotaError;
use crate::message::OriginQuotaLaneConfig;
use crate::message::Transaction;
use crate::message::DEFAULT_ORIGIN_QUOTA_BYTE_BURST;
use crate::message::DEFAULT_ORIGIN_QUOTA_MESSAGE_BURST;
use crate::message::DELEGATED_MIN_CHARGE;
use crate::storage::MemStorage;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;
use crate::tests::fixed_secret_keys;
use crate::tests::TEST_NETWORK_ID;

/// Payload prefix of the namespace the test application declares delegated admission for.
const DELEGATED_PREFIX: &[u8] = b"delegated/";

/// The message bucket of a record whose message limit was skipped: untouched.
fn messages_skipped() -> u128 {
    u128::from(DEFAULT_ORIGIN_QUOTA_MESSAGE_BURST)
}

/// The message bucket of a record charged one message.
fn messages_charged() -> u128 {
    messages_skipped() - 1
}

/// A stand-in for the application registry: payloads under [`DELEGATED_PREFIX`] belong to a
/// namespace that declared delegated admission, every other payload to one that did not.
#[derive(Default)]
struct DelegatingCallback {
    /// How often core consulted the registry.
    consulted: AtomicUsize,
}

#[async_trait]
impl SwarmCallback for DelegatingCallback {
    fn delegates_admission(&self, application_payload: &[u8]) -> bool {
        self.consulted.fetch_add(1, Ordering::SeqCst);
        application_payload.starts_with(DELEGATED_PREFIX)
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
    app: Arc<DelegatingCallback>,
}

impl Harness {
    /// A fresh local swarm owned by `key`, under `quota`.
    fn new(key: &SecretKey, quota: OriginQuotaConfig) -> Result<Self> {
        let local = DelegateeKey::new_with_seckey(key)?;
        let app = Arc::new(DelegatingCallback::default());
        let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
            .origin_quota(quota)
            .callback(app.clone())
            .build();
        let transport = Arc::clone(&swarm.transport);
        Ok(Self {
            swarm,
            transport,
            app,
        })
    }

    /// A fresh local swarm owned by `key`, under the default quota.
    fn with_defaults(key: &SecretKey) -> Result<Self> {
        Self::new(key, OriginQuotaConfig::default())
    }

    /// A peer owned by `key` whose connection is authenticated when `authenticated` holds.
    async fn peer(&self, key: &SecretKey, authenticated: bool) -> Result<Peer> {
        let did: Did = key.address().into();
        let session = DelegateeKey::new_with_seckey(key)?;
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

    /// Deliver `frame` over `carrier`'s connection.
    async fn deliver(&self, carrier: &Peer, frame: &bytes::Bytes) -> Result<()> {
        carrier
            .callback
            .on_admitted_message_for_test(&carrier.did.to_string(), frame)
            .await
            .map_err(|error| match error.downcast::<Error>() {
                Ok(error) => *error,
                Err(error) => Error::InvalidMessage(error.to_string()),
            })
    }

    /// `(message, byte)` tokens of `origin`'s Application record after its last admission.
    async fn tokens(&self, origin: Did) -> Option<(u128, u128)> {
        self.transport
            .origin_quota_tokens_for_test(origin, MessageCategory::Application)
            .await
    }
}

/// A frame whose transaction `origin` signed and whose hop `carrier` signed, for `local`,
/// with its byte cost (the transaction's `data` length).
fn wire(
    payload: &[u8],
    origin: &DelegateeKey,
    carrier: &DelegateeKey,
    local: Did,
    sequence: u64,
) -> Result<(bytes::Bytes, u128)> {
    let transaction = Transaction::new(
        local,
        uuid::Uuid::new_v4(),
        sequence,
        None,
        Message::custom(payload)?,
        MessageSigner::new(origin, TEST_NETWORK_ID),
    )?;
    let cost = u128::try_from(transaction.data.len()).map_err(|_| Error::MessageSizeOverflow)?;
    let frame = MessagePayload::new(
        transaction,
        MessageSigner::new(carrier, TEST_NETWORK_ID),
        MessageRelay::new(NextHop::toward(local), local, HopBudget::MAX),
    )?
    .to_wire()?;
    Ok((frame, cost))
}

/// The byte bucket after one enforced admission of `cost` from a full default bucket.
fn bytes_after(cost: u128) -> u128 {
    u128::from(DEFAULT_ORIGIN_QUOTA_BYTE_BURST) - cost
}

/// The byte bucket after one delegated admission of `cost`: the floor applies.
fn bytes_after_delegated(cost: u128) -> u128 {
    let floor = u128::try_from(DELEGATED_MIN_CHARGE).expect("usize fits u128");
    bytes_after(cost.max(floor))
}

#[tokio::test]
async fn test_neighbours_own_delegated_traffic_skips_only_the_message_limit() -> Result<()> {
    let [local, neighbour] = fixed_secret_keys::<2>()?;
    let harness = Harness::with_defaults(&local)?;
    let neighbour = harness.peer(&neighbour, true).await?;
    let (frame, cost) = wire(
        b"delegated/cell",
        &neighbour.session,
        &neighbour.session,
        harness.swarm.did(),
        0,
    )?;
    harness.deliver(&neighbour, &frame).await?;
    assert_eq!(
        harness.tokens(neighbour.did).await,
        Some((messages_skipped(), bytes_after_delegated(cost)))
    );
    Ok(())
}

#[tokio::test]
async fn test_neighbours_other_namespace_keeps_the_message_limit() -> Result<()> {
    let [local, neighbour] = fixed_secret_keys::<2>()?;
    let harness = Harness::with_defaults(&local)?;
    let neighbour = harness.peer(&neighbour, true).await?;
    let (frame, cost) = wire(
        b"other/message",
        &neighbour.session,
        &neighbour.session,
        harness.swarm.did(),
        0,
    )?;
    harness.deliver(&neighbour, &frame).await?;
    assert_eq!(
        harness.tokens(neighbour.did).await,
        Some((messages_charged(), bytes_after(cost)))
    );
    Ok(())
}

#[tokio::test]
async fn test_relayed_origin_keeps_the_message_limit() -> Result<()> {
    let [local, neighbour, relayed] = fixed_secret_keys::<3>()?;
    let harness = Harness::with_defaults(&local)?;
    let neighbour = harness.peer(&neighbour, true).await?;
    let relayed = DelegateeKey::new_with_seckey(&relayed)?;
    let (frame, cost) = wire(
        b"delegated/cell",
        &relayed,
        &neighbour.session,
        harness.swarm.did(),
        0,
    )?;
    harness.deliver(&neighbour, &frame).await?;
    assert_eq!(
        harness.tokens(relayed.delegator_did()).await,
        Some((messages_charged(), bytes_after(cost)))
    );
    // Ineligible traffic never reaches the application registry.
    assert_eq!(harness.app.consulted.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn test_neighbours_origin_over_another_neighbour_keeps_the_message_limit() -> Result<()> {
    let [local, origin, carrier] = fixed_secret_keys::<3>()?;
    let harness = Harness::with_defaults(&local)?;
    let origin = harness.peer(&origin, true).await?;
    let carrier = harness.peer(&carrier, true).await?;
    let (frame, cost) = wire(
        b"delegated/cell",
        &origin.session,
        &carrier.session,
        harness.swarm.did(),
        0,
    )?;
    harness.deliver(&carrier, &frame).await?;
    assert_eq!(
        harness.tokens(origin.did).await,
        Some((messages_charged(), bytes_after(cost)))
    );
    Ok(())
}

#[tokio::test]
async fn test_unauthenticated_peer_claiming_its_origin_keeps_the_message_limit() -> Result<()> {
    let [local, stranger] = fixed_secret_keys::<2>()?;
    let harness = Harness::with_defaults(&local)?;
    let stranger = harness.peer(&stranger, false).await?;
    let (frame, cost) = wire(
        b"delegated/cell",
        &stranger.session,
        &stranger.session,
        harness.swarm.did(),
        0,
    )?;
    harness.deliver(&stranger, &frame).await?;
    assert_eq!(
        harness.tokens(stranger.did).await,
        Some((messages_charged(), bytes_after(cost)))
    );
    assert_eq!(harness.app.consulted.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn test_reassembled_neighbour_traffic_skips_only_the_message_limit() -> Result<()> {
    let [local, neighbour] = fixed_secret_keys::<2>()?;
    let harness = Harness::with_defaults(&local)?;
    let neighbour = harness.peer(&neighbour, true).await?;
    let local = harness.swarm.did();
    let original = MessagePayload::new_send(
        Message::custom(&[DELEGATED_PREFIX, &[7; 512]].concat())?,
        MessageSigner::new(&neighbour.session, TEST_NETWORK_ID),
        local,
        local,
    )?;
    let cost =
        u128::try_from(original.transaction.data.len()).map_err(|_| Error::MessageSizeOverflow)?;
    let chunks: Vec<Chunk> = Chunk::stream(original.to_wire()?, 64).collect();
    assert!(chunks.len() > 1);
    for chunk in chunks {
        let frame = local_wire(Message::Chunk(chunk), &neighbour.session, local)?;
        harness.deliver(&neighbour, &frame).await?;
    }
    assert_eq!(
        harness.tokens(neighbour.did).await,
        Some((messages_skipped(), bytes_after_delegated(cost)))
    );
    Ok(())
}

/// Delegation skips the message limit, never the byte bucket: a small delegated message is
/// charged the floor, so a byte burst of one floor and a bit admits exactly one of them, where
/// its length alone would admit dozens. An undecodable payload in a delegating namespace is
/// just such a small message.
#[tokio::test]
async fn test_small_delegated_messages_are_charged_the_floor() -> Result<()> {
    let [local, neighbour] = fixed_secret_keys::<2>()?;
    let payload = [DELEGATED_PREFIX, &[0xff; 64]].concat();
    let byte_burst =
        u64::try_from(DELEGATED_MIN_CHARGE + 1_024).map_err(|_| Error::MessageSizeOverflow)?;
    let lane = OriginQuotaLaneConfig::new(1, 1, 1, byte_burst, 8)
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    let harness = Harness::new(&local, OriginQuotaConfig::new(lane, lane, lane, lane))?;
    let neighbour = harness.peer(&neighbour, true).await?;
    let mut refusals = Vec::new();
    for sequence in 0..2 {
        let (frame, _cost) = wire(
            &payload,
            &neighbour.session,
            &neighbour.session,
            harness.swarm.did(),
            sequence,
        )?;
        refusals.push(harness.deliver(&neighbour, &frame).await.err());
    }
    // A message burst of 1 would refuse the second message on messages if it applied.
    assert!(refusals.first().is_some_and(Option::is_none));
    assert!(matches!(
        refusals.get(1),
        Some(Some(Error::OriginQuota(
            OriginQuotaError::ByteRateExhausted { .. }
        )))
    ));
    Ok(())
}
