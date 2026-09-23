use super::*;
use crate::message::HopBudget;
use crate::message::MessageCategory;
use crate::message::MessageRelay;
use crate::message::OriginQuotaConfig;
use crate::message::OriginQuotaError;
use crate::message::OriginQuotaLaneConfig;
use crate::message::Transaction;

fn relayed_wire(
    message: Message,
    origin: &SessionSk,
    carrier: &SessionSk,
    local: Did,
    sequence: u64,
) -> Result<bytes::Bytes> {
    let transaction = Transaction::new(
        local,
        uuid::Uuid::new_v4(),
        sequence,
        message,
        MessageSigner::new(origin, TEST_NETWORK_ID),
    )?;
    MessagePayload::new(
        transaction,
        MessageSigner::new(carrier, TEST_NETWORK_ID),
        MessageRelay::new(local, local, HopBudget::MAX),
    )?
    .to_wire()
}

#[tokio::test]
async fn quota_is_shared_across_relays_but_isolated_between_origins() -> Result<()> {
    let lane = OriginQuotaLaneConfig::new(1, 1, 1024, 1024, 8)
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    let quota = OriginQuotaConfig::new(lane, lane, lane, lane);
    let local = SessionSk::new_with_seckey(&SecretKey::random())?;
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
        .origin_quota(quota)
        .callback(app_callback.clone())
        .build();
    let transport = Arc::clone(&swarm.transport);

    let relay_a_key = SecretKey::random();
    let relay_a: Did = relay_a_key.address().into();
    let relay_a_session = SessionSk::new_with_seckey(&relay_a_key)?;
    let relay_b_key = SecretKey::random();
    let relay_b: Did = relay_b_key.address().into();
    let relay_b_session = SessionSk::new_with_seckey(&relay_b_key)?;
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt_a, _offer) = transport
        .prepare_connection_offer_with_attempt(relay_a, offer_callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt_a)?);
    let callback_a = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt_a);
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt_b, _offer) = transport
        .prepare_connection_offer_with_attempt(relay_b, offer_callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt_b)?);
    let callback_b = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt_b);

    let origin_a = SessionSk::new_with_seckey(&SecretKey::random())?;
    let first = relayed_wire(
        Message::custom(b"origin-a-relay-a")?,
        &origin_a,
        &relay_a_session,
        swarm.did(),
        0,
    )?;
    callback_a
        .on_admitted_message_for_test(&relay_a.to_string(), &first)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    let second = relayed_wire(
        Message::custom(b"origin-a-relay-b")?,
        &origin_a,
        &relay_b_session,
        swarm.did(),
        1,
    )?;
    let exhausted = callback_b
        .on_admitted_message_for_test(&relay_b.to_string(), &second)
        .await
        .expect_err("a second relay path must share origin A's exhausted bucket");
    assert!(matches!(
        exhausted.downcast_ref::<Error>(),
        Some(Error::OriginQuota(
            OriginQuotaError::MessageRateExhausted { .. }
        ))
    ));

    let origin_b = SessionSk::new_with_seckey(&SecretKey::random())?;
    let independent = relayed_wire(
        Message::custom(b"origin-b-relay-a")?,
        &origin_b,
        &relay_a_session,
        swarm.did(),
        0,
    )?;
    callback_a
        .on_admitted_message_for_test(&relay_a.to_string(), &independent)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(app_callback.validates(), 2);
    assert_eq!(app_callback.inbounds(), 2);
    assert!(transport.is_active_connection_attempt(attempt_a));
    assert!(transport.is_active_connection_attempt(attempt_b));
    assert_eq!(
        swarm
            .origin_quota_counters()
            .lane(MessageCategory::Application)
            .message_rate_exhausted,
        1
    );
    Ok(())
}

#[tokio::test]
async fn normal_and_reassembled_messages_each_consume_one_logical_byte_cost() -> Result<()> {
    let local = SessionSk::new_with_seckey(&SecretKey::random())?;
    let local_did = local.account_did();
    let origin = SessionSk::new_with_seckey(&SecretKey::random())?;
    let message = Message::custom(&vec![41; 512])?;
    let reassembled = MessagePayload::new_send(
        message.clone(),
        MessageSigner::new(&origin, TEST_NETWORK_ID),
        local_did,
        local_did,
    )?;
    let normal = MessagePayload::new_send(
        message.clone(),
        MessageSigner::new(&origin, TEST_NETWORK_ID),
        local_did,
        local_did,
    )?;
    let exhausted = MessagePayload::new_send(
        message,
        MessageSigner::new(&origin, TEST_NETWORK_ID),
        local_did,
        local_did,
    )?;
    let logical_cost = u64::try_from(reassembled.transaction.data.len())
        .map_err(|_| Error::MessageSizeOverflow)?;
    assert_eq!(
        normal.transaction.data.len(),
        reassembled.transaction.data.len()
    );
    let byte_burst = logical_cost
        .checked_mul(2)
        .ok_or(Error::MessageSizeOverflow)?;
    let lane = OriginQuotaLaneConfig::new(10, 10, 1, byte_burst, 8)
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    let quota = OriginQuotaConfig::new(lane, lane, lane, lane);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
        .origin_quota(quota)
        .callback(app_callback.clone())
        .build();
    let transport = Arc::clone(&swarm.transport);
    let relay_key = SecretKey::random();
    let relay: Did = relay_key.address().into();
    let relay_session = SessionSk::new_with_seckey(&relay_key)?;
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(relay, offer_callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt);

    let chunks: Vec<Chunk> = Chunk::stream(reassembled.to_wire()?, 32).collect();
    assert!(chunks.len() > 1);
    let chunk_count = chunks.len();
    for chunk in chunks {
        let frame = local_wire(Message::Chunk(chunk), &relay_session, local_did)?;
        callback
            .on_admitted_message_for_test(&relay.to_string(), &frame)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    }
    callback
        .on_admitted_message_for_test(&relay.to_string(), &normal.to_wire()?)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    let error = callback
        .on_admitted_message_for_test(&relay.to_string(), &exhausted.to_wire()?)
        .await
        .expect_err("two logical costs must exhaust the configured byte burst");

    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::OriginQuota(
            OriginQuotaError::ByteRateExhausted { .. }
        ))
    ));
    assert_eq!(app_callback.validates(), chunk_count.saturating_add(2));
    assert_eq!(app_callback.inbounds(), 2);
    assert_eq!(
        swarm
            .origin_quota_counters()
            .lane(MessageCategory::Application)
            .byte_rate_exhausted,
        1
    );
    assert!(transport.is_active_connection_attempt(attempt));
    Ok(())
}
