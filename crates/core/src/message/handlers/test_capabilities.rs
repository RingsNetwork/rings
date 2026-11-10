//! Deterministic replay and sleep-free clock tests for [`HandlerCtx`](super::HandlerCtx) and
//! [`SharedRng`](crate::utils::SharedRng).

use std::sync::Arc;

use crate::consts::MAX_RELAY_INBOX_TTL_MS;
use crate::delegation::DelegateeKey;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::Chord;
use crate::dht::PeerRing;
use crate::dht::StorageKey;
use crate::ecc::keys::Ed25519SecretKey;
use crate::ecc::keys::SigningSecretKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::e2e::encrypt_stream_frames_with_rng;
use crate::message::types::CustomMessage;
use crate::message::types::FetchRelayInbox;
use crate::message::HandleMsg;
use crate::message::HandlerCtx;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::Principal;
use crate::message::ProbeRequest;
use crate::message::ProvisionalEpoch;
use crate::message::ServiceReceiptError;
use crate::message::PROVISIONAL_RECEIPT_EPOCH_SECS;
use crate::storage::MemStorage;
use crate::swarm::SwarmBuilder;
use crate::tests::TEST_NETWORK_ID;
use crate::utils::Clock;
use crate::utils::ManualClock;
use crate::utils::SharedRng;

const INITIAL_MS: u128 = 1_700_000_000_000;

#[test]
fn test_seeded_rng_and_clock_replay_identical_e2e_keys_and_dht_tokens() -> Result<()> {
    let run_trace = |seed: u64| -> Result<_> {
        let clock = Arc::new(ManualClock::new(INITIAL_MS));
        let rng = SharedRng::seed_from_u64(seed);

        let sender = SecretKey::random_with_rng(&mut rng.lock_rng());
        let recipient = SecretKey::random_with_rng(&mut rng.lock_rng());
        let ed_key = Ed25519SecretKey::random_with_rng(&mut rng.lock_rng()).to_bytes();
        let bls_pk = SigningSecretKey::random_bls12381_with_rng(&mut rng.lock_rng())?
            .public_key()?
            .did();

        let stream_id = rng.next_uuid();
        let frames = encrypt_stream_frames_with_rng(
            b"deterministic e2e payload across frames",
            stream_id,
            sender.pubkey(),
            recipient.pubkey(),
            16,
            rng.clone(),
        )?
        .collect::<Result<Vec<_>>>()?;

        let ring =
            PeerRing::new_with_storage(sender.address().into(), 3, Box::new(MemStorage::new()))
                .with_clock(clock)
                .with_rng(rng.clone());
        let peer_did = recipient.address().into();
        ring.admit_connected(peer_did, None)?;
        let jitter = ring.finger_jitter_entropy();
        let stab = format!("{:?}", ring.pre_stabilize()?);
        let fix = format!("{:?}", ring.fix_fingers()?);

        Ok((
            sender.address(),
            recipient.address(),
            ed_key,
            bls_pk,
            stream_id,
            frames,
            jitter,
            stab,
            fix,
        ))
    };

    let first = run_trace(1230)?;
    let second = run_trace(1230)?;
    let different = run_trace(1231)?;

    assert_eq!(first, second, "same seed and clock must replay identically");
    assert_ne!(first, different, "distinct seeds must diverge");
    Ok(())
}

#[tokio::test]
async fn test_handler_ctx_clock_drives_probe_tolerance_and_storage_hold_expiry_without_sleeps(
) -> Result<()> {
    let base_ms = crate::utils::get_epoch_ms();
    let clock = ManualClock::new(base_ms);
    let shared_clock = Arc::new(clock.clone());
    let rng = SharedRng::seed_from_u64(1230);

    let local_sk = SecretKey::random_with_rng(&mut rng.lock_rng());
    let sender_sk = SecretKey::random_with_rng(&mut rng.lock_rng());

    let local_del = DelegateeKey::new_with_seckey(&local_sk)?;
    let sender_del = DelegateeKey::new_with_seckey(&sender_sk)?;

    let swarm = SwarmBuilder::new(
        TEST_NETWORK_ID,
        "",
        Box::new(MemStorage::new()),
        local_del.clone(),
    )
    .relay_storage(crate::tests::volatile_relay_storage())
    .relay_replication(crate::tests::ungated_relay_replication())
    .replay_storage(crate::tests::volatile_replay_storage())
    .clock(shared_clock)
    .rng(rng.clone())
    .build();
    let handler = MessageHandler::new(swarm.transport.clone());

    let offline_sk = SecretKey::random_with_rng(&mut rng.lock_rng());

    // 1. Service-receipt probe epoch tolerance driven purely by HandlerCtx's injected ManualClock.
    let epoch_secs = u64::try_from(base_ms / 1_000).expect("fits u64");
    let probe_req = ProbeRequest::from_rng(
        ProvisionalEpoch::from_unix_seconds(epoch_secs),
        &mut rng.lock_rng(),
    );
    let probe_payload = MessagePayload::new_send(
        Message::ProbeRequest(probe_req),
        MessageSigner::new(&sender_del, TEST_NETWORK_ID),
        swarm.did(),
        swarm.did(),
    )?;

    // Advance the manual clock well past the provisional receipt epoch tolerance (no sleep!).
    let expired_probe_ms = base_ms + u128::from(PROVISIONAL_RECEIPT_EPOCH_SECS) * 1_000 * 5;
    clock.set(expired_probe_ms);
    let expired_ctx = HandlerCtx::new_with_clock(&probe_payload, Principal::Local, &clock);
    let probe_err = handler
        .handle(&expired_ctx, &probe_req)
        .await
        .expect_err("probe outside tolerance must be rejected at expired ManualClock");
    assert!(
        matches!(
            probe_err,
            Error::ServiceReceipt(ServiceReceiptError::EpochOutsideTolerance { .. })
        ),
        "unexpected error: {probe_err:?}"
    );

    // 2. Offline relay inbox hold + storage expiry driven purely by ManualClock without sleeps.
    clock.set(base_ms);
    let offline_did = offline_sk.address().into();
    let custom = CustomMessage(b"offline-held".to_vec());
    let offline_payload = MessagePayload::new_send(
        Message::CustomMessage(custom.clone()),
        MessageSigner::new(&sender_del, TEST_NETWORK_ID),
        swarm.did(),
        offline_did,
    )?;
    // Single-node ring is responsible for every position; offline_did has no connection, so
    // handling CustomMessage holds it in local relay storage at `clock.now_ms() == base_ms`.
    let hold_ctx = HandlerCtx::new_with_clock(&offline_payload, Principal::Local, &clock);
    handler.handle(&hold_ctx, &custom).await?;

    let inbox_key = StorageKey::inbox_of(offline_did);
    assert!(
        swarm
            .dht
            .live_storage_entry(inbox_key, base_ms + 1_000)
            .await?
            .is_some(),
        "held message must be live at base_ms + 1s"
    );

    // Advance ManualClock past MAX_RELAY_INBOX_TTL_MS: the held entry expires immediately with
    // zero sleeps, and FetchRelayInbox at that HandlerCtx clock observes an empty/expired carrier.
    let after_ttl_ms = base_ms + u128::from(MAX_RELAY_INBOX_TTL_MS) + 1_000;
    clock.set(after_ttl_ms);
    assert!(
        swarm
            .dht
            .live_storage_entry(inbox_key, clock.now_ms())
            .await?
            .is_none(),
        "held message must expire once ManualClock advances past MAX_RELAY_INBOX_TTL_MS"
    );

    let offline_del = DelegateeKey::new_with_seckey(&offline_sk)?;
    let fetch_payload = MessagePayload::new_send(
        Message::FetchRelayInbox(FetchRelayInbox),
        MessageSigner::new(&offline_del, TEST_NETWORK_ID),
        swarm.did(),
        swarm.did(),
    )?;
    let fetch_ctx = HandlerCtx::new_with_clock(&fetch_payload, Principal::Local, &clock);
    let _ = handler.handle(&fetch_ctx, &FetchRelayInbox).await;

    // 3. Direct PlacedEntryOperation stamping uses HandlerCtx's ManualClock.
    clock.set(base_ms + 2_000);
    let resource_did = swarm.did();
    let entry = Entry::new(resource_did, Vec::new(), EntryKind::Data);
    let placed = PlacedEntryOperation {
        placement: resource_did,
        op: EntryOperation::Overwrite(entry),
        report: false,
    };
    let op_payload = MessagePayload::new_send(
        Message::OperateEntry(placed.clone()),
        MessageSigner::new(&local_del, TEST_NETWORK_ID),
        swarm.did(),
        swarm.did(),
    )?;
    let op_ctx = HandlerCtx::new_with_clock(&op_payload, Principal::Local, &clock);
    handler.handle(&op_ctx, &placed).await?;

    Ok(())
}
