use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rand::rngs::OsRng;
use rings_core::delegation::DelegateeKey;
use rings_core::ecc::SecretKey;
use rings_core::utils::get_epoch_ms;
use tokio::time::Instant;

use super::read_delegator_secret_key;
use super::run_signing_horizon;
use super::signing_horizon;
use super::write_delegatee_key_atomically;
use super::DelegationRenewalPaths;
use super::HORIZON_POLL;
use crate::error::Error;
use crate::mail::descriptor::MailRecipient;
use crate::mail::envelope::seal;
use crate::mail::open::open_mail_with_keys;
use crate::mail::open::Opening;
use crate::processor::DelegationHorizon;
use crate::processor::Processor;
use crate::processor::ProcessorConfig;
use crate::processor::SIGNING_HORIZON_LEAD_MS;
use crate::test_support::TestTask;
use crate::tests::test_processor_builder;
use crate::tests::TEST_ICE_SERVERS;
use crate::tests::TEST_NETWORK_ID;

/// The epoch reading at the paused runtime's start.
const EPOCH_MS: u128 = 1_800_000_000_000;

/// An epoch clock that reads the paused runtime's time from `EPOCH_MS`, plus `jump_ms` once
/// `after` of it has passed: a host suspended at `after` for `jump_ms`, whose monotonic
/// clock (the paused runtime's) did not advance meanwhile.
fn injected_clock(after: Duration, jump_ms: u128) -> impl Fn() -> u128 {
    let start = Instant::now();
    move || {
        let elapsed = start.elapsed();
        let jumped = if elapsed >= after { jump_ms } else { 0 };
        EPOCH_MS + elapsed.as_millis() + jumped
    }
}

/// Build a test processor whose initial delegation is stamped at `ts_ms` with `ttl_ms`.
fn processor_at(delegator: &SecretKey, ttl_ms: u64, ts_ms: u128) -> Arc<Processor> {
    let delegatee_key = match DelegateeKey::new_with_seckey_at(delegator, ttl_ms, ts_ms) {
        Ok(key) => key,
        Err(error) => panic!("test delegatee key mint must succeed: {error}"),
    };
    let config = ProcessorConfig::new(
        TEST_NETWORK_ID,
        TEST_ICE_SERVERS.to_string(),
        delegatee_key,
        3,
    );
    let processor = match test_processor_builder(&config).build() {
        Ok(processor) => processor,
        Err(error) => panic!("test processor build must succeed: {error}"),
    };
    Arc::new(processor)
}

/// `signing → mute` (#1063): a delegation two leads past the start is waited out on the
/// injected clock, in sleeps of at most `P`, and the daemon's error names the horizon at
/// exactly that instant. The health predicate (`overlay_ready` reads `signs_at`) holds one
/// millisecond before and fails at the end.
#[tokio::test(start_paused = true)]
async fn test_the_daemon_ends_at_its_horizon() {
    let now_ms = injected_clock(Duration::MAX, 0);
    let horizon = DelegationHorizon::expiring_at(EPOCH_MS + 2 * SIGNING_HORIZON_LEAD_MS);
    let h = horizon.horizon_ms();

    let error = signing_horizon(horizon, &now_ms).await;

    assert!(matches!(
        error,
        Error::DelegationHorizonReached { horizon_ms, .. } if horizon_ms == h
    ));
    assert_eq!(now_ms(), h);
    assert!(horizon.signs_at(h - 1) && !horizon.signs_at(h));
}

/// A host suspended across the horizon (#1138 review): ten seconds in, the epoch clock jumps
/// three leads, past the horizon, while the monotonic clock does not move. The daemon ends
/// at its next wake, within one poll period of the start, not after the day-long sleep the
/// monotonic clock alone would have waited.
#[tokio::test(start_paused = true)]
async fn test_a_daemon_suspended_across_its_horizon_ends_within_one_poll() {
    let suspended_at = Duration::from_secs(10);
    let now_ms = injected_clock(suspended_at, 3 * SIGNING_HORIZON_LEAD_MS);
    let horizon = DelegationHorizon::expiring_at(EPOCH_MS + 2 * SIGNING_HORIZON_LEAD_MS);
    let start = Instant::now();

    let error = signing_horizon(horizon, &now_ms).await;

    assert!(matches!(error, Error::DelegationHorizonReached { .. }));
    assert!(start.elapsed() > suspended_at && start.elapsed() <= HORIZON_POLL);
}

/// A daemon already past its horizon ends at once, without waiting.
#[tokio::test(start_paused = true)]
async fn test_a_daemon_past_its_horizon_ends_at_once() {
    let now_ms = injected_clock(Duration::MAX, 0);
    let horizon = DelegationHorizon::expiring_at(EPOCH_MS);

    let error = signing_horizon(horizon, &now_ms).await;

    assert!(matches!(error, Error::DelegationHorizonReached { .. }));
    assert_eq!(now_ms(), EPOCH_MS);
}

/// In-process delegation renewal (#1153): a node with `delegator_key` configured renews at
/// `h − R` across multiple horizons (`h0 < h1 < h2`), atomically persists each new
/// `DelegateeKey` (`0600`, no leftover temp files), updates both `Processor` and `Swarm`,
/// immediately republishes its online descriptor with the new `delegatee_public_key`, opens
/// mail sealed to the prior key before `e0`, and falls back to exiting at `h2` if the
/// delegator key is removed before the next renewal.
#[tokio::test(start_paused = true)]
async fn test_run_signing_horizon_renews_in_process_across_multiple_horizons_and_republishes() {
    let base_ms = get_epoch_ms().max(EPOCH_MS);
    let start = Instant::now();
    let now_ms = move || base_ms + start.elapsed().as_millis();

    let delegator = SecretKey::random();
    let ttl_ms = u64::try_from(3 * SIGNING_HORIZON_LEAD_MS).unwrap_or(u64::MAX);
    let processor = processor_at(&delegator, ttl_ms, base_ms);
    let initial_key = processor.delegatee_key();
    let initial_pubkey = initial_key.delegatee_public_key();
    let h0 = processor.signing_horizon().horizon_ms();
    let e0 = processor.signing_horizon().expires_at_ms();

    let root = std::env::temp_dir().join(format!("rings-horizon-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).expect("create temp horizon dir");
    let delegator_key_path = root.join("ecdsa_key");
    let delegatee_key_path = root.join("delegatee_key");
    fs::write(
        &delegator_key_path,
        format!("# rings init secret key\n0x{}\n", delegator.to_string()),
    )
    .expect("write delegator key");
    assert_eq!(
        read_delegator_secret_key(&delegator_key_path).expect("read delegator key"),
        delegator
    );
    write_delegatee_key_atomically(&delegatee_key_path, &initial_key)
        .expect("write initial delegatee key");

    // Publish initial online-node descriptor before renewal.
    processor.republish_registrations().await;
    let initial_online = processor
        .lookup_online_nodes(false)
        .await
        .expect("list initial online nodes");
    assert_eq!(
        initial_online
            .iter()
            .find(|node| node.did == processor.did())
            .map(|node| node.delegatee_public_key),
        Some(initial_pubkey)
    );

    // Seal mail to the initial key before renewal so we can verify decryption after rotation.
    let recipient_before_renewal = MailRecipient {
        did: processor.did(),
        encryption_key: initial_pubkey,
    };
    let mail_bytes = b"Message-ID: <across-renewal@example.org>\r\n\r\nStill readable.\r\n";
    let sealed_to_initial =
        seal(mail_bytes, &recipient_before_renewal, &mut OsRng).expect("seal mail to initial key");

    let paths = DelegationRenewalPaths {
        delegatee_key_path: delegatee_key_path.clone(),
        delegator_key_path: delegator_key_path.clone(),
    };
    let task_processor = Arc::clone(&processor);
    let mut horizon_task =
        TestTask::spawn(
            async move { run_signing_horizon(task_processor, Some(paths), now_ms).await },
        );

    // Advance to `h0 - R + 1 ms`: first renewal must have completed.
    let to_first_renewal =
        u64::try_from((h0 - SIGNING_HORIZON_LEAD_MS) - base_ms + 1).unwrap_or(u64::MAX);
    tokio::time::sleep(Duration::from_millis(to_first_renewal)).await;
    tokio::task::yield_now().await;

    let h1 = processor.signing_horizon().horizon_ms();
    let k1 = processor.delegatee_key();
    assert!(
        h1 > h0,
        "Monotone horizon law: h1 ({h1}) must exceed h0 ({h0})"
    );
    assert_ne!(k1.delegatee_public_key(), initial_pubkey);
    assert_eq!(k1.delegator_did(), processor.did());
    assert_eq!(
        processor.swarm.delegatee_key().delegatee_public_key(),
        k1.delegatee_public_key()
    );

    // On-disk delegatee_key was atomically updated with mode 0600 and no leftover temp files.
    let disk_raw_1 = fs::read_to_string(&delegatee_key_path).expect("read renewed key 1");
    let disk_key_1 = DelegateeKey::from_str(&disk_raw_1).expect("parse renewed key 1");
    assert_eq!(disk_key_1.delegatee_public_key(), k1.delegatee_public_key());
    #[cfg(unix)]
    {
        let mode = fs::metadata(&delegatee_key_path)
            .expect("stat delegatee_key")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    let leftover_temps = fs::read_dir(&root)
        .expect("read temp dir")
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
        .count();
    assert_eq!(leftover_temps, 0);

    // Online-node descriptor was republished immediately with k1's delegatee_public_key.
    let online_after_1 = processor
        .lookup_online_nodes(false)
        .await
        .expect("list online nodes after first renewal");
    assert_eq!(
        online_after_1
            .iter()
            .find(|node| node.did == processor.did())
            .map(|node| node.delegatee_public_key),
        Some(k1.delegatee_public_key())
    );

    // Advance past the original horizon `h0`: the node is still signing, holds a valid
    // delegation, and opens mail sealed to `k0` before `e0`.
    let past_h0 = u64::try_from(h0 + 1 - now_ms()).unwrap_or(0);
    tokio::time::sleep(Duration::from_millis(past_h0)).await;
    assert!(
        !horizon_task.is_finished(),
        "node must keep running past h0"
    );
    assert!(processor.signing_horizon().signs_at(now_ms()));
    let sig = processor
        .swarm
        .delegatee_key()
        .sign(b"frame-past-h0")
        .expect("sign past h0");
    assert!(processor
        .delegatee_key()
        .delegation()
        .verify_at(b"frame-past-h0", sig, now_ms())
        .is_ok());
    assert!(now_ms() < e0);
    let opened = open_mail_with_keys(
        &sealed_to_initial.elements,
        processor.did(),
        &processor.decryption_keys_at(now_ms()),
        |_| true,
    );
    assert!(matches!(
        opened.as_slice(),
        [(Opening::Opened(ref mail), Some(by_key))]
            if mail.message.as_slice() == mail_bytes && *by_key == initial_pubkey
    ));

    // Advance to `h1 - R + 1 ms`: second renewal must complete (`h2 > h1 > h0`).
    let to_second_renewal =
        u64::try_from((h1 - SIGNING_HORIZON_LEAD_MS) - now_ms() + 1).unwrap_or(0);
    tokio::time::sleep(Duration::from_millis(to_second_renewal)).await;
    tokio::task::yield_now().await;

    let h2 = processor.signing_horizon().horizon_ms();
    let k2 = processor.delegatee_key();
    assert!(
        h2 > h1,
        "Monotone horizon law: h2 ({h2}) must exceed h1 ({h1})"
    );
    assert_ne!(k2.delegatee_public_key(), k1.delegatee_public_key());
    let snapshot = processor
        .operator_snapshot()
        .await
        .expect("operator snapshot");
    assert_eq!(snapshot.session_key.rotation_succeeded_total, 2);
    assert_eq!(snapshot.session_key.rotation_failed_total, 0);

    let online_after_2 = processor
        .lookup_online_nodes(false)
        .await
        .expect("list online nodes after second renewal");
    assert_eq!(
        online_after_2
            .iter()
            .find(|node| node.did == processor.did())
            .map(|node| node.delegatee_public_key),
        Some(k2.delegatee_public_key())
    );

    // Remove the delegator key file before `h2 - R`: the third renewal fails and falls back
    // to exiting at `h2` with `Error::DelegationHorizonReached`.
    fs::remove_file(&delegator_key_path).expect("remove delegator key for fallback test");
    let exit_error = (&mut horizon_task)
        .await
        .expect("horizon task joins cleanly");
    assert!(matches!(
        exit_error,
        Error::DelegationHorizonReached { horizon_ms, .. } if horizon_ms == h2
    ));
    assert_eq!(now_ms(), h2);
    let snapshot_end = processor
        .operator_snapshot()
        .await
        .expect("operator snapshot at exit");
    assert_eq!(snapshot_end.session_key.rotation_succeeded_total, 2);
    assert_eq!(snapshot_end.session_key.rotation_failed_total, 1);

    let _ = fs::remove_dir_all(&root);
}

/// Fallback law (#1153): without `delegator_key` configured, or when `delegator_key` has a
/// DID different from the node's DID, `run_signing_horizon` keeps the old key and exits at
/// `h` with `Error::DelegationHorizonReached` (#1063 behaviour).
#[tokio::test(start_paused = true)]
async fn test_run_signing_horizon_falls_back_when_delegator_key_is_absent_or_mismatched() {
    let ttl_ms = u64::try_from(2 * SIGNING_HORIZON_LEAD_MS).unwrap_or(u64::MAX);

    // 1. No `delegator_key` configured (`renewal = None`).
    {
        let now_ms = injected_clock(Duration::MAX, 0);
        let delegator = SecretKey::random();
        let processor = processor_at(&delegator, ttl_ms, EPOCH_MS);
        let h0 = processor.signing_horizon().horizon_ms();

        let error = run_signing_horizon(Arc::clone(&processor), None, &now_ms).await;
        assert!(matches!(
            error,
            Error::DelegationHorizonReached { horizon_ms, .. } if horizon_ms == h0
        ));
        assert_eq!(now_ms(), h0);
    }

    // 2. Mismatched `delegator_key` (different account DID).
    {
        let now_ms = injected_clock(Duration::MAX, 0);
        let delegator = SecretKey::random();
        let wrong_delegator = SecretKey::random();
        let processor = processor_at(&delegator, ttl_ms, EPOCH_MS);
        let initial_pubkey = processor.delegatee_key().delegatee_public_key();
        let h0 = processor.signing_horizon().horizon_ms();

        let root =
            std::env::temp_dir().join(format!("rings-horizon-fallback-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("create temp fallback dir");
        let delegator_key_path = root.join("wrong_ecdsa_key");
        let delegatee_key_path = root.join("delegatee_key");
        fs::write(&delegator_key_path, wrong_delegator.to_string())
            .expect("write wrong delegator key");
        write_delegatee_key_atomically(&delegatee_key_path, &processor.delegatee_key())
            .expect("write initial delegatee key");

        let paths = DelegationRenewalPaths {
            delegatee_key_path: delegatee_key_path.clone(),
            delegator_key_path,
        };
        let error = run_signing_horizon(Arc::clone(&processor), Some(paths), &now_ms).await;
        assert!(matches!(
            error,
            Error::DelegationHorizonReached { horizon_ms, .. } if horizon_ms == h0
        ));
        assert_eq!(now_ms(), h0);
        assert_eq!(
            processor.delegatee_key().delegatee_public_key(),
            initial_pubkey
        );
        let snapshot = processor
            .operator_snapshot()
            .await
            .expect("operator snapshot");
        assert_eq!(snapshot.session_key.rotation_succeeded_total, 0);
        assert_eq!(snapshot.session_key.rotation_failed_total, 1);

        let _ = fs::remove_dir_all(&root);
    }
}

/// Short-TTL delegation renewal (#1153): a delegation whose lifetime is shorter than
/// `SIGNING_HORIZON_LEAD_MS` starts in `SigningState::Expiring` at `t = c0`, waits until its
/// half-life `c0 + (h0 − c0) / 2` before renewing to `h1 > h0` with zero failed attempts, and
/// does not churn keys before `c1 + (h1 − c1) / 2`.
#[tokio::test(start_paused = true)]
async fn test_run_signing_horizon_renews_short_ttl_delegation_at_half_life_without_churn() {
    let base_ms = get_epoch_ms().max(EPOCH_MS);
    let start = Instant::now();
    let now_ms = move || base_ms + start.elapsed().as_millis();

    let delegator = SecretKey::random();
    let short_ttl_ms: u64 = 600_000;
    let processor = processor_at(&delegator, short_ttl_ms, base_ms);
    let initial_key = processor.delegatee_key();
    let initial_pubkey = initial_key.delegatee_public_key();
    let h0 = processor.signing_horizon().horizon_ms();
    let first_renew_at_ms = base_ms + (h0 - base_ms) / 2;

    let root = std::env::temp_dir().join(format!("rings-horizon-short-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).expect("create temp short-ttl dir");
    let delegator_key_path = root.join("ecdsa_key");
    let delegatee_key_path = root.join("delegatee_key");
    fs::write(&delegator_key_path, delegator.to_string()).expect("write delegator key");
    write_delegatee_key_atomically(&delegatee_key_path, &initial_key)
        .expect("write initial delegatee key");

    let paths = DelegationRenewalPaths {
        delegatee_key_path,
        delegator_key_path: delegator_key_path.clone(),
    };
    let task_processor = Arc::clone(&processor);
    let mut horizon_task =
        TestTask::spawn(
            async move { run_signing_horizon(task_processor, Some(paths), now_ms).await },
        );

    // One millisecond before the half-life `first_renew_at_ms`, the node has neither renewed
    // nor recorded a failed attempt at `t = c0`.
    let before_first = u64::try_from(first_renew_at_ms - 1 - base_ms).unwrap_or(0);
    tokio::time::sleep(Duration::from_millis(before_first)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        processor.delegatee_key().delegatee_public_key(),
        initial_pubkey
    );
    let snap_before = processor
        .operator_snapshot()
        .await
        .expect("snapshot before first renewal");
    assert_eq!(snap_before.session_key.rotation_succeeded_total, 0);
    assert_eq!(snap_before.session_key.rotation_failed_total, 0);

    // Advance 2 ms across `first_renew_at_ms`: exactly one renewal occurs (`h1 > h0`).
    tokio::time::sleep(Duration::from_millis(2)).await;
    tokio::task::yield_now().await;
    let k1 = processor.delegatee_key();
    let h1 = processor.signing_horizon().horizon_ms();
    assert!(h1 > h0);
    assert_ne!(k1.delegatee_public_key(), initial_pubkey);
    let snap_after_1 = processor
        .operator_snapshot()
        .await
        .expect("snapshot after first renewal");
    assert_eq!(snap_after_1.session_key.rotation_succeeded_total, 1);
    assert_eq!(snap_after_1.session_key.rotation_failed_total, 0);

    // One poll period later (still before `c1 + (h1 - c1) / 2`), the key has not churned.
    tokio::time::sleep(HORIZON_POLL).await;
    tokio::task::yield_now().await;
    assert_eq!(
        processor.delegatee_key().delegatee_public_key(),
        k1.delegatee_public_key()
    );
    let snap_mid = processor
        .operator_snapshot()
        .await
        .expect("snapshot mid-window");
    assert_eq!(snap_mid.session_key.rotation_succeeded_total, 1);
    assert_eq!(snap_mid.session_key.rotation_failed_total, 0);

    // Remove the delegator key before the second renewal and verify clean fallback exit at `h1`.
    fs::remove_file(&delegator_key_path).expect("remove delegator key");
    let exit_error = (&mut horizon_task)
        .await
        .expect("horizon task joins cleanly");
    assert!(matches!(
        exit_error,
        Error::DelegationHorizonReached { horizon_ms, .. } if horizon_ms == h1
    ));
    assert_eq!(now_ms(), h1);

    let _ = fs::remove_dir_all(&root);
}
