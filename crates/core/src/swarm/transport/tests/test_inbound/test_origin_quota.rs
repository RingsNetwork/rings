use super::*;
use crate::message::MessageCategory;
use crate::message::OriginQuotaConfig;
use crate::message::OriginQuotaError;
use crate::message::OriginQuotaLaneConfig;

#[tokio::test]
async fn quota_is_shared_across_relays_but_isolated_between_origins() -> Result<()> {
    let lane = OriginQuotaLaneConfig::new(1, 1, 1024, 1024, 8)
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    let quota = OriginQuotaConfig::new(lane, lane, lane, lane);
    let local = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
        .origin_quota(quota)
        .build();
    let transport = Arc::clone(&swarm.transport);
    let application = ApplicationRecorder::attach(&transport)?;

    let relay_a = pending_peer(&transport).await?;
    relay_a.activate(&transport)?;
    let relay_b = pending_peer(&transport).await?;
    relay_b.activate(&transport)?;

    let origin_a = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let relayed = |message: &[u8], origin: &DelegateeKey, relay: &PendingPeer, sequence| {
        relayed_payload(
            Message::custom(message)?,
            origin,
            &relay.session,
            swarm.did(),
            sequence,
        )?
        .to_wire()
    };
    relay_a
        .receive(&relayed(b"origin-a-relay-a", &origin_a, &relay_a, 0)?)
        .await?;
    let second = relayed(b"origin-a-relay-b", &origin_a, &relay_b, 1)?;
    let exhausted = relay_b
        .callback
        .on_admitted_message_for_test(&relay_b.peer.to_string(), &second)
        .await
        .expect_err("a second relay path must share origin A's exhausted bucket");
    assert!(matches!(
        exhausted.downcast_ref::<Error>(),
        Some(Error::OriginQuota(
            OriginQuotaError::MessageRateExhausted { .. }
        ))
    ));

    let origin_b = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    relay_a
        .receive(&relayed(b"origin-b-relay-a", &origin_b, &relay_a, 0)?)
        .await?;

    assert_eq!(application.observed(), 2);
    assert_eq!(application.inbounds(), 2);
    assert!(transport.is_active_connection_attempt(relay_a.attempt()?));
    assert!(transport.is_active_connection_attempt(relay_b.attempt()?));
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
    let local = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let local_did = local.delegator_did();
    let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let message = Message::custom(&vec![41; 512])?;
    let reassembled = local_payload(message.clone(), &origin, local_did)?;
    let normal = local_payload(message.clone(), &origin, local_did)?;
    let exhausted = local_payload(message, &origin, local_did)?;
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
    let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
        .origin_quota(quota)
        .build();
    let transport = Arc::clone(&swarm.transport);
    let application = ApplicationRecorder::attach(&transport)?;
    let relay = pending_peer(&transport).await?;
    relay.activate(&transport)?;

    let (last, chunk_count) = deliver_leading_chunks(
        &relay.callback,
        &relay.peer.to_string(),
        &relay.session,
        local_did,
        reassembled.to_wire()?,
    )
    .await?;
    relay.receive(&last).await?;
    relay.receive(&normal.to_wire()?).await?;
    let error = relay
        .callback
        .on_admitted_message_for_test(&relay.peer.to_string(), &exhausted.to_wire()?)
        .await
        .expect_err("two logical costs must exhaust the configured byte burst");

    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::OriginQuota(
            OriginQuotaError::ByteRateExhausted { .. }
        ))
    ));
    assert_eq!(application.observed(), chunk_count.saturating_add(2));
    assert_eq!(application.inbounds(), 2);
    assert_eq!(
        swarm
            .origin_quota_counters()
            .lane(MessageCategory::Application)
            .byte_rate_exhausted,
        1
    );
    assert!(transport.is_active_connection_attempt(relay.attempt()?));
    Ok(())
}
