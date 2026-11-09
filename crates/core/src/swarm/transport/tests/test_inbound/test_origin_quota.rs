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
        .replay_storage(crate::tests::volatile_replay_storage())
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
        .replay_storage(crate::tests::volatile_replay_storage())
        .origin_quota(quota)
        .build();
    let transport = Arc::clone(&swarm.transport);
    let application = ApplicationRecorder::attach(&transport)?;
    let relay = pending_peer(&transport).await?;
    relay.activate(&transport)?;

    let (last, chunk_count) = deliver_leading_chunks(
        &relay.callback,
        (&relay.peer.to_string(), None),
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

/// Issue #1219 (1): A non-redirectable frame (`CustomMessage`) signed for a dummy
/// `transaction.destination != local_did` but carrying `relay.destination == local_did` is
/// rejected during frame verification (both whole and chunked/reassembled) before reaching
/// admission or dispatch.
#[tokio::test]
async fn spoofed_transaction_destination_with_local_relay_destination_is_rejected() -> Result<()> {
    let local = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let local_did = local.delegator_did();
    let dummy_destination: Did = SecretKey::random().address().into();
    let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
        .replay_storage(crate::tests::volatile_replay_storage())
        .build();
    let transport = Arc::clone(&swarm.transport);
    let application = ApplicationRecorder::attach(&transport)?;
    let relay = pending_peer(&transport).await?;
    relay.activate(&transport)?;

    let spoofed_tx = Transaction::new(
        dummy_destination,
        crate::utils::new_uuid(),
        0,
        None,
        Message::custom(&vec![7; 128])?,
        MessageSigner::new(&relay.session, TEST_NETWORK_ID),
    )?;
    let spoofed = MessagePayload::new(
        spoofed_tx,
        MessageSigner::new(&relay.session, TEST_NETWORK_ID),
        MessageRelay::new(NextHop::toward(local_did), local_did, HopBudget::MAX),
    )?;

    let direct_err = relay
        .callback
        .on_admitted_message_for_test(&relay.peer.to_string(), &spoofed.to_wire()?)
        .await
        .expect_err("spoofed relay.destination != transaction.destination must be rejected");
    assert!(matches!(
        direct_err.downcast_ref::<Error>(),
        Some(Error::InvalidMessage(_))
    ));

    let (last_chunk, _chunk_count) = deliver_leading_chunks(
        &relay.callback,
        (&relay.peer.to_string(), None),
        &relay.session,
        local_did,
        spoofed.to_wire()?,
    )
    .await?;
    let chunked_err = relay
        .callback
        .on_admitted_message_for_test(&relay.peer.to_string(), &last_chunk)
        .await
        .expect_err("reassembled spoofed payload must also be rejected");
    assert!(matches!(
        chunked_err.downcast_ref::<Error>(),
        Some(Error::InvalidMessage(_))
    ));

    assert_eq!(application.inbounds(), 0);
    assert_eq!(transport.origin_quota_record_count_for_test().await, 0);
    Ok(())
}

/// Issue #1219 (2): Multi-hop `reset_destination` frames (`FindSuccessorSend`, `SearchEntry`,
/// `OperateEntry`, `FindRelayHolder`) terminating at the local node enforce `TransactionReplay`,
/// `OriginQuotaTable` (keyed by the local terminating node across distinct first-hop
/// `transaction.destination`s), and receiver `SlotAccounting`.
#[tokio::test]
async fn redirected_frames_enforce_replay_and_origin_quota_at_terminating_node() -> Result<()> {
    use crate::dht::entry::EntryKind;
    use crate::dht::entry::EntryOperation;
    use crate::dht::entry::PlacedEntryOperation;
    use crate::message::FindSuccessorReportHandler;
    use crate::message::FindSuccessorSend;
    use crate::message::FindSuccessorThen;
    use crate::message::SearchEntry;

    let lane = OriginQuotaLaneConfig::new(1, 2, 64 * 1024, 64 * 1024, 8)
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    let quota = OriginQuotaConfig::new(lane, lane, lane, lane);
    let local = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let local_did = local.delegator_did();
    let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
        .dht_virtual_nodes(0)
        .dht_storage_redundancy(1)
        .replay_storage(crate::tests::volatile_replay_storage())
        .origin_quota(quota)
        .build();
    let transport = Arc::clone(&swarm.transport);
    let relay = pending_peer(&transport).await?;
    relay.admit(&transport).await?;
    *swarm.dht().lock_predecessor()? = Some(local_did - Did::from(100u32));

    let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let origin_did = origin.delegator_did();
    let first_hop_a: Did = SecretKey::random().address().into();
    let first_hop_b: Did = SecretKey::random().address().into();

    let redirected_wire =
        |msg: Message, tx_dest: Did, relay_dest: Did, sequence: u64| -> Result<Bytes> {
            let tx = Transaction::new(
                tx_dest,
                crate::utils::new_uuid(),
                sequence,
                None,
                msg,
                MessageSigner::new(&origin, TEST_NETWORK_ID),
            )?;
            MessagePayload::new(
                tx,
                MessageSigner::new(&relay.session, TEST_NETWORK_ID),
                MessageRelay::new(NextHop::toward(local_did), relay_dest, HopBudget::MAX),
            )?
            .to_wire()
        };

    // 1. Redirected SearchEntry with relay.destination == local_did and transaction.destination == first_hop_a.
    let search_a = redirected_wire(
        Message::SearchEntry(SearchEntry {
            resource: local_did,
            placement: local_did,
            redundancy: 1,
        }),
        first_hop_a,
        local_did,
        1,
    )?;
    relay.receive(&search_a).await?;

    // Replaying the exact redirected SearchEntry must be rejected by TransactionReplay.
    let replay_err = relay
        .callback
        .on_admitted_message_for_test(&relay.peer.to_string(), &search_a)
        .await
        .expect_err("replayed redirected SearchEntry must be rejected");
    assert!(matches!(
        replay_err.downcast_ref::<Error>(),
        Some(Error::TransactionReplay { .. })
    ));
    assert_eq!(swarm.transaction_replay_counters().replay, 1);

    // 2. Redirected OperateEntry from the same origin but via a DIFFERENT first hop (`first_hop_b`)
    // and with `relay.destination != local_did` (terminated locally by DHT key ownership since
    // `swarm` stands alone and owns `local_did`).
    let entry = crate::tests::live_entry(
        local_did,
        vec![Bytes::from_static(b"redirected-write")],
        EntryKind::Data,
    );
    let operate_b = redirected_wire(
        Message::OperateEntry(PlacedEntryOperation {
            placement: local_did,
            op: EntryOperation::Overwrite(entry.clone()),
            report: false,
        }),
        first_hop_b,
        first_hop_b,
        1,
    )?;
    relay.receive(&operate_b).await?;

    // 3. A third Storage-lane redirected request from the same origin must exhaust the local
    // terminating node's quota bucket (burst = 2), even when using a third `transaction.destination`.
    let first_hop_c: Did = SecretKey::random().address().into();
    let exhausted_storage = redirected_wire(
        Message::SearchEntry(SearchEntry {
            resource: local_did,
            placement: local_did,
            redundancy: 1,
        }),
        first_hop_c,
        local_did,
        1,
    )?;
    let quota_err = relay
        .callback
        .on_admitted_message_for_test(&relay.peer.to_string(), &exhausted_storage)
        .await
        .expect_err("redirected storage frames across distinct first hops must share origin quota");
    assert!(matches!(
        quota_err.downcast_ref::<Error>(),
        Some(Error::OriginQuota(
            OriginQuotaError::MessageRateExhausted { .. }
        ))
    ));
    assert_eq!(
        swarm
            .origin_quota_counters()
            .lane(MessageCategory::Storage)
            .message_rate_exhausted,
        1
    );
    assert_eq!(
        transport
            .origin_quota_tokens_for_test(origin_did, MessageCategory::Storage)
            .await
            .map(|(messages, _bytes)| messages),
        Some(0)
    );

    // 4. Redirected FindSuccessorSend in DhtControl lane also enforces replay protection.
    let find_succ = redirected_wire(
        Message::FindSuccessorSend(FindSuccessorSend {
            did: local_did,
            strict: false,
            then: FindSuccessorThen::Report(FindSuccessorReportHandler::None),
        }),
        first_hop_a,
        local_did,
        1,
    )?;
    relay.receive(&find_succ).await?;
    let find_succ_replay = relay
        .callback
        .on_admitted_message_for_test(&relay.peer.to_string(), &find_succ)
        .await
        .expect_err("replayed redirected FindSuccessorSend must be rejected");
    assert!(matches!(
        find_succ_replay.downcast_ref::<Error>(),
        Some(Error::TransactionReplay { .. })
    ));
    assert_eq!(swarm.transaction_replay_counters().replay, 2);
    Ok(())
}

/// Issue #1219 (3): `CustomMessage` addressed to an offline peer and intercepted by a relay
/// holder via `hold_for_offline_destination` must undergo final transaction admission
/// (`TransactionReplay` and `OriginQuotaTable` keyed by the holding node) before being held.
#[tokio::test]
async fn offline_inbox_hold_enforces_final_admission_at_intercepting_holder() -> Result<()> {
    let lane = OriginQuotaLaneConfig::new(1, 1, 64 * 1024, 64 * 1024, 8)
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    let quota = OriginQuotaConfig::new(lane, lane, lane, lane);
    let local = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let local_did = local.delegator_did();
    let swarm = SwarmBuilder::new(TEST_NETWORK_ID, "", Box::new(MemStorage::new()), local)
        .relay_storage(Box::new(MemStorage::new()))
        .relay_replication(crate::dht::RelayReplication {
            replicas: 1,
            age_gate_ms: 0,
        })
        .replay_storage(crate::tests::volatile_replay_storage())
        .origin_quota(quota)
        .build();
    let transport = Arc::clone(&swarm.transport);
    let relay = pending_peer(&transport).await?;
    relay.activate(&transport)?;

    // Because `swarm` stands alone (`predecessor == None`, no successors) and holds relay mail,
    // `swarm` is responsible for both `offline_a` and `offline_b` and intercepts `CustomMessage`
    // via `hold_for_offline_destination` into its own relay store.
    let offline_a = local_did - Did::from(50u32);
    let offline_b = local_did - Did::from(25u32);

    let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let origin_did = origin.delegator_did();
    let offline_wire = |offline_dest: Did, sequence: u64| -> Result<Bytes> {
        let tx = Transaction::new(
            offline_dest,
            crate::utils::new_uuid(),
            sequence,
            None,
            Message::custom(b"for-offline-peer")?,
            MessageSigner::new(&origin, TEST_NETWORK_ID),
        )?;
        MessagePayload::new(
            tx,
            MessageSigner::new(&relay.session, TEST_NETWORK_ID),
            MessageRelay::new(NextHop::toward(local_did), offline_dest, HopBudget::MAX),
        )?
        .to_wire()
    };

    let first_hold = offline_wire(offline_a, 0)?;
    relay.receive(&first_hold).await?;
    assert_eq!(
        transport
            .origin_quota_tokens_for_test(origin_did, MessageCategory::Application)
            .await
            .map(|(messages, _bytes)| messages),
        Some(0)
    );

    // Replaying the held message at the holder must be rejected by TransactionReplay.
    let replay_err = relay
        .callback
        .on_admitted_message_for_test(&relay.peer.to_string(), &first_hold)
        .await
        .expect_err("replayed offline-held CustomMessage must be rejected at holder");
    assert!(matches!(
        replay_err.downcast_ref::<Error>(),
        Some(Error::TransactionReplay { .. })
    ));
    assert_eq!(swarm.transaction_replay_counters().replay, 1);

    // A second message from the same origin to a DIFFERENT offline destination held by `swarm`
    // must share the holder's OriginQuotaKey and be refused once burst = 1 is exhausted.
    let second_hold = offline_wire(offline_b, 0)?;
    let quota_err = relay
        .callback
        .on_admitted_message_for_test(&relay.peer.to_string(), &second_hold)
        .await
        .expect_err("offline holds across distinct destinations must share origin quota at holder");
    assert!(matches!(
        quota_err.downcast_ref::<Error>(),
        Some(Error::OriginQuota(
            OriginQuotaError::MessageRateExhausted { .. }
        ))
    ));
    assert_eq!(
        swarm
            .origin_quota_counters()
            .lane(MessageCategory::Application)
            .message_rate_exhausted,
        1
    );
    Ok(())
}
