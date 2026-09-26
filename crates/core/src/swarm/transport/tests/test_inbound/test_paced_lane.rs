//! End-to-end admission of the paced direct-edge lane (#888).
//!
//! One authenticated neighbour sends Application traffic of a namespace with a registered
//! paced lane and of one without; a relayed origin sends paced traffic through the same
//! neighbour. Only the neighbour's own paced traffic may leave the default Application quota.
//!
//! Admission reads the runtime's monotonic clock, so the default lane refills slightly while a
//! test runs: these tests assert which lane refuses, and the exact rate bounds are checked on a
//! simulated clock in the quota module.

use std::num::NonZeroU64;

use super::*;
use crate::dht::delivery::NextHop;
use crate::message::EdgeRelation;
use crate::message::HopBudget;
use crate::message::MessageRelay;
use crate::message::OriginQuotaError;
use crate::message::PacedLane;
use crate::message::PacedLaneId;
use crate::message::PacedRate;
use crate::message::Transaction;
use crate::message::DEFAULT_ORIGIN_QUOTA_MESSAGE_BURST;

/// Payload prefix the test application registers a paced lane for.
const PACED_PREFIX: &[u8] = b"paced/";

/// Messages each sender emits: twice the default Application burst.
const SENDS: u64 = 2 * DEFAULT_ORIGIN_QUOTA_MESSAGE_BURST;

/// A stand-in for the application registry: payloads under [`PACED_PREFIX`] belong to one
/// paced lane, every other payload to none.
#[derive(Default)]
struct PacedRegistryCallback {
    /// Application messages delivered past admission.
    inbounds: AtomicUsize,
}

impl PacedRegistryCallback {
    /// The lane the test application registers, at the onion data plane's `B/V`.
    fn lane() -> PacedLane {
        PacedLane::new(
            PacedLaneId::new(1),
            PacedRate::new(
                NonZeroU64::new(16_384).unwrap(),
                NonZeroU64::new(150).unwrap(),
            ),
        )
    }
}

#[async_trait]
impl SwarmCallback for PacedRegistryCallback {
    async fn on_inbound(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        self.inbounds.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn paced_lane(&self, application_payload: &[u8]) -> Option<PacedLane> {
        application_payload
            .starts_with(PACED_PREFIX)
            .then(Self::lane)
    }
}

/// A frame whose transaction `origin` signed and whose hop `carrier` signed, for `local`.
fn wire(
    payload: &[u8],
    origin: &DelegateeKey,
    carrier: &DelegateeKey,
    local: Did,
    sequence: u64,
) -> Result<bytes::Bytes> {
    let transaction = Transaction::new(
        local,
        uuid::Uuid::new_v4(),
        sequence,
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

/// Admit `SENDS` messages of `payload` from `origin` over the neighbour's connection and
/// return how many the quota refused.
async fn refusals(
    callback: &InnerSwarmCallback,
    neighbour: Did,
    origin: &DelegateeKey,
    carrier: &DelegateeKey,
    local: Did,
    payload: &[u8],
) -> Result<u64> {
    let mut refused = 0;
    for sequence in 0..SENDS {
        let frame = wire(payload, origin, carrier, local, sequence)?;
        match callback
            .on_admitted_message_for_test(&neighbour.to_string(), &frame)
            .await
        {
            Ok(()) => {}
            Err(error) => match error.downcast_ref::<Error>() {
                Some(Error::OriginQuota(OriginQuotaError::MessageRateExhausted { .. })) => {
                    refused += 1
                }
                _ => return Err(Error::InvalidMessage(error.to_string())),
            },
        }
    }
    Ok(refused)
}

#[tokio::test]
async fn test_paced_lane_admits_only_the_neighbours_own_registered_traffic() -> Result<()> {
    let local = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let app = Arc::new(PacedRegistryCallback::default());
    let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
        .callback(app.clone())
        .build();
    let transport = Arc::clone(&swarm.transport);

    let neighbour_key = SecretKey::random();
    let neighbour: Did = neighbour_key.address().into();
    let neighbour_session = DelegateeKey::new_with_seckey(&neighbour_key)?;
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(neighbour, offer_callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), app.clone())
        .with_pending_connection_attempt(attempt);
    assert_eq!(
        EdgeRelation::of(Some(neighbour), neighbour_session.delegator_did()),
        EdgeRelation::Neighbour
    );

    // The neighbour's own paced traffic: the protocol's rate, not the default burst.
    let paced = refusals(
        &callback,
        neighbour,
        &neighbour_session,
        &neighbour_session,
        swarm.did(),
        b"paced/cell",
    )
    .await?;
    assert_eq!(paced, 0);

    // A relayed origin's paced traffic through the same neighbour: the default lane.
    let relayed_origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let relayed = refusals(
        &callback,
        neighbour,
        &relayed_origin,
        &neighbour_session,
        swarm.did(),
        b"paced/cell",
    )
    .await?;
    assert!(
        relayed > 0,
        "a relayed origin must not claim the paced lane"
    );

    let counters = swarm.origin_quota_counters();
    assert_eq!(counters.paced(), Default::default());
    assert_eq!(
        counters
            .lane(MessageCategory::Application)
            .message_rate_exhausted,
        relayed
    );
    Ok(())
}

#[tokio::test]
async fn test_unregistered_namespace_of_the_neighbour_keeps_the_default_quota() -> Result<()> {
    let local = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let app = Arc::new(PacedRegistryCallback::default());
    let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
        .callback(app.clone())
        .build();
    let transport = Arc::clone(&swarm.transport);

    let neighbour_key = SecretKey::random();
    let neighbour: Did = neighbour_key.address().into();
    let neighbour_session = DelegateeKey::new_with_seckey(&neighbour_key)?;
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(neighbour, offer_callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), app.clone())
        .with_pending_connection_attempt(attempt);

    let other = refusals(
        &callback,
        neighbour,
        &neighbour_session,
        &neighbour_session,
        swarm.did(),
        b"other/message",
    )
    .await?;

    assert!(
        other > 0,
        "an unregistered namespace keeps the default quota"
    );
    assert_eq!(
        u64::try_from(app.inbounds.load(Ordering::SeqCst)).unwrap(),
        SENDS - other
    );
    let counters = swarm.origin_quota_counters();
    assert_eq!(counters.paced(), Default::default());
    assert_eq!(
        counters
            .lane(MessageCategory::Application)
            .message_rate_exhausted,
        other
    );
    Ok(())
}
