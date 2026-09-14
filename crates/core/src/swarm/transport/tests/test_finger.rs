//! Transport-level tests for finger-fix reports that depend on connection lifecycle state.

use super::*;
use crate::dht::finger::FingerConvergencePhase;
use crate::dht::FingerFixRequest;

/// Prepare the exact request token that production finger lookup would place in the DHT.
///
/// The helper mutates the test finger state into `AwaitingReport` for `slot` and
/// returns an error when that slot cannot start a request, ensuring each test
/// exercises the same correlation checks as the message handler.
fn finger_request(transport: &SwarmTransport, slot: usize) -> Result<FingerFixRequest> {
    transport
        .dht
        .lock_finger()?
        .prepare_request_for_test(slot)
        .ok_or_else(|| Error::InvalidMessage("failed to prepare test finger request".to_owned()))
}

/// Prove that a proof queued by a pending generation is applied by its admission commit.
///
/// The test also checks that the proof remains absent from the finger table
/// before commit and moves through the DHT's `AwaitingAdmission` phase.
#[tokio::test(start_paused = true)]
async fn test_pending_finger_update_is_applied_when_attempt_is_admitted() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let finger_index = 0;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;

    assert_eq!(
        transport.record_finger_candidate(peer, finger_request(&transport, finger_index)?)?,
        FingerUpdateDisposition::Queued
    );
    assert_eq!(transport.dht.lock_finger()?.get(finger_index), None);
    assert!(matches!(
        transport
            .dht
            .lock_finger()?
            .convergence_state()
            .status()
            .phase(),
        crate::dht::finger::FingerConvergencePhase::AwaitingAdmission { .. }
    ));
    // Move past the retry cooldown while staying inside the shared admission timeout.
    tokio::time::advance(std::time::Duration::from_millis(11_000)).await;
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;

    // Reuse the original attempt token so the synthetic data-channel callback
    // can promote only the generation that queued the finger proof.
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback))
        .with_pending_connection_attempt(attempt);
    callback
        .on_data_channel_open(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(transport.dht.lock_finger()?.get(finger_index), Some(peer));
    assert!(transport.is_admitted_connection(peer));

    transport.disconnect(peer).await?;
    Ok(())
}

/// Prove that cancelling a pending handshake also cancels its deferred finger proof.
///
/// The expected failure streak witnesses that cancellation reaches the DHT
/// convergence state instead of merely deleting the transport-side queue.
#[tokio::test]
async fn test_pending_handshake_cancellation_releases_its_finger_proof() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;

    // The queued request transfers from AwaitingReport to AwaitingAdmission
    // and should be cancelled when the pending handshake is cancelled.
    let request = finger_request(&transport, 0)?;
    assert_eq!(
        transport.record_finger_candidate(peer, request)?,
        FingerUpdateDisposition::Queued
    );
    assert!(transport.cancel_pending_connection(attempt).await?);

    let finger = transport.dht.lock_finger()?;
    assert_eq!(finger.get(0), None);
    assert_eq!(finger.convergence_state().status().failure_streak(), 1);
    Ok(())
}

/// Prove that an admitting generation retains a finger proof until atomic commit.
///
/// The candidate must stay out of the finger table while admission is in
/// progress and become visible only after the same generation commits.
#[tokio::test]
async fn test_admitting_finger_update_is_retained_until_atomic_commit() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let finger_index = 0;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;

    assert!(transport.begin_connection_admission_for_test(attempt)?);
    let observed = transport
        .unadmitted_attempt(peer)?
        .ok_or_else(|| Error::InvalidMessage("admitting attempt disappeared".to_string()))?;
    assert_eq!(observed, attempt);
    assert_eq!(
        transport.record_finger_candidate(peer, finger_request(&transport, finger_index)?)?,
        FingerUpdateDisposition::Queued
    );
    assert_eq!(transport.dht.lock_finger()?.get(finger_index), None);

    assert!(transport.commit_connection_admission(attempt)?.is_some());

    assert_eq!(transport.dht.lock_finger()?.get(finger_index), Some(peer));
    assert!(transport.is_admitted_connection_attempt(attempt));
    transport.disconnect(peer).await?;
    Ok(())
}

/// Prove that a candidate applies immediately when admission wins the queue race.
///
/// The active generation is made fully routable before the report arrives, so
/// the report must bypass deferred storage and update the requested slot.
#[tokio::test]
async fn test_pending_finger_update_applies_if_admission_wins_queue_race() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let finger_index = 0;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;

    assert!(transport.activate_connection_for_test(attempt)?);
    transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Connected)?;
    transport.force_peer_data_channel_open_without_callback(peer, Some(true))?;
    assert_eq!(
        transport.record_finger_candidate(peer, finger_request(&transport, finger_index)?)?,
        FingerUpdateDisposition::Applied
    );

    assert_eq!(transport.dht.lock_finger()?.get(finger_index), Some(peer));
    assert!(transport.is_admitted_connection(peer));
    Ok(())
}

/// Prove that missing and owned-but-unroutable peers produce distinct outcomes.
///
/// Missing peers retain a proof that can move onto a new pending generation;
/// unroutable active peers retire the proof and increment convergence failure.
#[tokio::test(start_paused = true)]
async fn test_finger_candidate_distinguishes_missing_and_unroutable_connections() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let missing = SecretKey::random().address().into();
    // The same request is reused after the caller opens a missing connection;
    // this models the handler's second admission attempt after `connect_dht_peer`.
    let missing_request = finger_request(&transport, 0)?;
    assert_eq!(
        transport.record_finger_candidate(missing, missing_request)?,
        FingerUpdateDisposition::Missing
    );
    assert!(matches!(
        transport
            .dht
            .lock_finger()?
            .convergence_state()
            .status()
            .phase(),
        FingerConvergencePhase::AwaitingAdmission { .. }
    ));

    tokio::time::advance(std::time::Duration::from_millis(11_000)).await;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (missing_attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(missing, callback)
        .await?;
    assert_eq!(
        transport.record_finger_candidate(missing, missing_request)?,
        FingerUpdateDisposition::Queued
    );
    assert!(transport.cancel_pending_connection(missing_attempt).await?);

    // Use a fresh DHT so the unroutable branch is not affected by the retained
    // proof and failure counter from the missing-connection branch above.
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);

    let unroutable_request = finger_request(&transport, 0)?;
    assert_eq!(
        transport.record_finger_candidate(peer, unroutable_request)?,
        FingerUpdateDisposition::Unroutable
    );
    assert_eq!(
        transport
            .dht
            .lock_finger()?
            .convergence_state()
            .status()
            .failure_streak(),
        1
    );
    assert_eq!(transport.dht.lock_finger()?.get(0), None);
    transport.disconnect(peer).await?;
    Ok(())
}

/// Prove that an expired request cannot authorize transport admission or finger mutation.
///
/// A second delivery of the same token must be stale, demonstrating that the
/// first rejection consumed the expired in-flight request deterministically.
#[tokio::test(start_paused = true)]
async fn test_expired_finger_candidate_never_enters_connection_admission() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    // Created before time advances; after the timeout it should be rejected
    // without creating any pending transport state.
    let request = finger_request(&transport, 0)?;

    tokio::time::advance(std::time::Duration::from_millis(11_000)).await;
    let expired = transport.record_finger_candidate(peer, request)?;

    assert_eq!(expired, FingerUpdateDisposition::Expired);
    assert!(!expired.needs_connection());
    assert!(transport.unadmitted_attempt(peer)?.is_none());
    assert!(transport.get_connection(peer).is_none());
    assert_eq!(transport.dht.lock_finger()?.get(0), None);
    assert_eq!(
        transport.record_finger_candidate(peer, request)?,
        FingerUpdateDisposition::Stale
    );
    Ok(())
}

/// Prove that a candidate below the requested threshold cannot open a connection.
///
/// The invalid report leaves transport and finger state unchanged, and replaying
/// its consumed request token is classified as stale.
#[tokio::test]
async fn test_invalid_finger_candidate_never_enters_connection_admission() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    // Slot 1's threshold is not proved by the immediate slot-0 successor, so
    // the report is invalid even though it names a plausible ring position.
    let peer = transport.dht.did + crate::dht::Did::power_of_two(0);
    let request = finger_request(&transport, 1)?;

    let invalid = transport.record_finger_candidate(peer, request)?;

    assert_eq!(invalid, FingerUpdateDisposition::Invalid);
    assert!(!invalid.needs_connection());
    assert!(transport.unadmitted_attempt(peer)?.is_none());
    assert!(transport.get_connection(peer).is_none());
    assert_eq!(transport.dht.lock_finger()?.get(1), None);
    assert_eq!(
        transport.record_finger_candidate(peer, request)?,
        FingerUpdateDisposition::Stale
    );
    Ok(())
}
