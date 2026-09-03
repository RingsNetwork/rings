#[cfg(feature = "dummy")]
use rings_transport::connections::dummy_controlled;

#[cfg(feature = "dummy")]
use super::super::delivery::SendCompletionOutcome;
use super::*;
#[cfg(feature = "dummy")]
use crate::dht::StorageSyncDestination;
#[cfg(feature = "dummy")]
use crate::dht::TopoInfo;

#[cfg(feature = "dummy")]
async fn transport_with_routable_peer(
) -> Result<(Arc<SwarmTransport>, Did, PendingConnectionAttempt)> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Connected)?;
    transport.force_peer_data_channel_open_without_callback(peer, Some(true))?;
    Ok((transport, peer, attempt))
}

#[cfg(feature = "dummy")]
struct BoundedThread<T> {
    completion: std::sync::mpsc::Receiver<T>,
    thread: std::thread::JoinHandle<()>,
}

#[cfg(feature = "dummy")]
impl<T: Send + 'static> BoundedThread<T> {
    fn spawn(operation: impl FnOnce() -> T + Send + 'static) -> Self {
        let (completion_tx, completion) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = operation();
            let _ = completion_tx.send(result);
        });
        Self { completion, thread }
    }

    fn finish(self, label: &'static str) -> Result<T> {
        let result = self
            .completion
            .recv_timeout(std::time::Duration::from_secs(1))
            .map_err(|error| Error::InvalidMessage(format!("{label} did not finish: {error}")))?;
        self.thread
            .join()
            .map_err(|_| Error::InvalidMessage(format!("{label} thread panicked")))?;
        Ok(result)
    }
}

#[cfg(feature = "dummy")]
struct LifecycleGateController {
    entered: std::sync::mpsc::Receiver<()>,
    release: std::sync::mpsc::SyncSender<()>,
}

#[cfg(feature = "dummy")]
impl LifecycleGateController {
    fn wait_until_entered(&self) -> Result<()> {
        self.entered
            .recv_timeout(std::time::Duration::from_secs(1))
            .map_err(|error| {
                Error::InvalidMessage(format!(
                    "lifecycle operation did not hold the gate: {error}"
                ))
            })
    }

    fn release(self) -> Result<()> {
        self.release.send(()).map_err(|error| {
            Error::InvalidMessage(format!("lifecycle gate release failed: {error}"))
        })
    }
}

#[cfg(feature = "dummy")]
fn lifecycle_test_gate() -> (impl FnOnce() + Send + 'static, LifecycleGateController) {
    let (entered_tx, entered) = std::sync::mpsc::sync_channel(0);
    let (release, release_rx) = std::sync::mpsc::sync_channel(0);
    let hold = move || {
        entered_tx
            .send(())
            .expect("lifecycle gate observer must remain open");
        release_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("lifecycle gate must be released within the test bound");
    };
    (hold, LifecycleGateController { entered, release })
}

#[cfg(feature = "dummy")]
struct BlockedRetirement {
    waiter_registered: std::sync::mpsc::Receiver<()>,
    worker: BoundedThread<Result<Option<()>>>,
}

#[cfg(feature = "dummy")]
impl BlockedRetirement {
    fn spawn(transport: Arc<SwarmTransport>, peer: Did, attempt: PendingConnectionAttempt) -> Self {
        let (waiter_tx, waiter_registered) = std::sync::mpsc::sync_channel(0);
        let action_transport = Arc::clone(&transport);
        let worker = BoundedThread::spawn(move || {
            transport.retire_active_connection_with_observer_for_test(
                attempt,
                || {
                    let _ = waiter_tx.send(());
                },
                |_| {
                    action_transport.dht.remove(peer)?;
                    Ok(())
                },
            )
        });
        Self {
            waiter_registered,
            worker,
        }
    }

    fn wait_until_registered(&self, transport: &SwarmTransport) -> Result<()> {
        self.waiter_registered
            .recv_timeout(std::time::Duration::from_secs(1))
            .map_err(|error| {
                Error::InvalidMessage(format!("retirement waiter was not registered: {error}"))
            })?;
        if transport.retirement_waiter_count_for_test() != 1 {
            return Err(Error::InvalidMessage(
                "retirement waiter count did not witness lifecycle contention".to_string(),
            ));
        }
        Ok(())
    }

    fn finish(self) -> Result<Option<()>> {
        self.worker.finish("generation retirement")?
    }
}

#[test]
fn test_connection_lifecycle_registry_is_bounded_and_rejects_duplicate_peers() -> Result<()> {
    let mut registry = ConnectionLifecycleRegistry::new(LifecycleBounds::new(2, 8));
    let now = 1_000;
    let peer_a = SecretKey::random().address().into();
    let peer_b = SecretKey::random().address().into();
    let peer_c = SecretKey::random().address().into();

    let attempt_a = registry.reserve(peer_a, now)?;
    assert!(matches!(
        registry.reserve(peer_a, now),
        Err(Error::AlreadyConnected)
    ));
    let _attempt_b = registry.reserve(peer_b, now)?;
    assert!(matches!(
        registry.reserve(peer_c, now),
        Err(Error::PendingConnectionCapacityExceeded { capacity: 2 })
    ));
    assert_eq!(registry.pending_len(), 2);

    assert!(registry.remove_pending(attempt_a));
    assert_eq!(registry.pending_len(), 1);
    assert!(registry.reserve(peer_c, now).is_ok());
    Ok(())
}

#[test]
fn test_connection_lifecycle_registry_bounds_records_across_every_phase() -> Result<()> {
    let mut registry = ConnectionLifecycleRegistry::new(LifecycleBounds::new(2, 2));
    let now = 1_000;
    let peer_a = SecretKey::random().address().into();
    let peer_b = SecretKey::random().address().into();
    let peer_c = SecretKey::random().address().into();

    let attempt_a = registry.reserve(peer_a, now)?;
    assert!(registry.begin_admission(attempt_a));
    let admitting = registry
        .admitting_connection(attempt_a)
        .ok_or(Error::AlreadyConnected)?;
    admitting.activate();
    let _attempt_b = registry.reserve(peer_b, now)?;
    assert_eq!(
        registry.reservation_verdict(peer_c),
        ReservationVerdict::CapacityExceeded
    );
    assert!(registry.reservation_verdict(peer_c).needs_eviction());
    assert_eq!(
        registry.reservation_verdict(peer_a),
        ReservationVerdict::AlreadyConnected
    );
    assert!(matches!(
        registry.reserve(peer_c, now),
        Err(Error::ConnectionCapacityExceeded { capacity: 2 })
    ));
    assert_eq!(registry.pending_len(), 1);

    assert!(registry.remove_active(attempt_a));
    assert_eq!(
        registry.reservation_verdict(peer_c),
        ReservationVerdict::Admissible
    );
    assert!(registry.reserve(peer_c, now).is_ok());
    // Two pending records saturate both bounds; the handshake bound is the
    // verdict because it is decided before the total.
    assert_eq!(
        registry.reservation_verdict(peer_a),
        ReservationVerdict::PendingCapacityExceeded
    );
    assert!(registry.reserve(peer_a, now).is_err());
    Ok(())
}

/// A saturated handshake set is refused before the total bound is consulted,
/// so it never asks for an eviction.
#[test]
fn test_pending_saturation_is_not_a_capacity_verdict() -> Result<()> {
    let mut registry = ConnectionLifecycleRegistry::new(LifecycleBounds::new(1, 3));
    let now = 1_000;
    let peer_a = SecretKey::random().address().into();
    let peer_b = SecretKey::random().address().into();

    let _attempt_a = registry.reserve(peer_a, now)?;
    let verdict = registry.reservation_verdict(peer_b);

    assert_eq!(verdict, ReservationVerdict::PendingCapacityExceeded);
    assert!(!verdict.needs_eviction());
    assert!(matches!(
        registry.reserve(peer_b, now),
        Err(Error::PendingConnectionCapacityExceeded { capacity: 1 })
    ));
    Ok(())
}

/// Law: the handshake share never exceeds the total.
#[test]
fn test_lifecycle_bounds_clamp_the_handshake_share_into_the_total() {
    let bounds = LifecycleBounds::new(8, 2);
    assert_eq!(bounds.pending(), 2);
    assert_eq!(bounds.total(), 2);
    assert_eq!(LifecycleBounds::new(2, 8).pending(), 2);
}

#[test]
fn test_stale_pending_callback_cannot_remove_a_replacement_attempt() -> Result<()> {
    let mut registry = ConnectionLifecycleRegistry::new(LifecycleBounds::new(1, 8));
    let now = 1_000;
    let peer = SecretKey::random().address().into();

    let old_attempt = registry.reserve(peer, now)?;
    assert!(registry.remove_pending(old_attempt));
    let current_attempt = registry.reserve(peer, now)?;

    assert!(!registry.remove_pending(old_attempt));
    assert!(registry.contains(peer));
    assert!(registry.remove_pending(current_attempt));
    Ok(())
}

#[test]
fn test_connection_lifecycle_registry_expires_only_unopened_handshakes() -> Result<()> {
    let mut registry = ConnectionLifecycleRegistry::new(LifecycleBounds::new(1, 8));
    let now = 1_000;
    let active_peer = SecretKey::random().address().into();
    let pending_peer = SecretKey::random().address().into();
    let active_attempt = registry.reserve(active_peer, now)?;
    assert!(registry.activate_for_test(active_attempt));
    let pending_attempt = registry.reserve(pending_peer, now)?;

    let expired = registry.expire(now + PENDING_CONNECTION_TIMEOUT_MS);
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].attempt, pending_attempt);
    assert_eq!(expired[0].age_ms, PENDING_CONNECTION_TIMEOUT_MS);
    assert_eq!(expired[0].phase.as_str(), "pending");
    assert_eq!(registry.pending_len(), 0);
    assert_eq!(registry.active_attempt(active_peer), Some(active_attempt));
    Ok(())
}

#[test]
fn test_connection_lifecycle_registry_expires_abandoned_admission() -> Result<()> {
    let mut registry = ConnectionLifecycleRegistry::new(LifecycleBounds::new(1, 8));
    let now = 1_000;
    let peer = SecretKey::random().address().into();
    let replacement_peer = SecretKey::random().address().into();
    let attempt = registry.reserve(peer, now)?;

    assert!(registry.begin_admission(attempt));
    assert_eq!(registry.admitting_attempt(peer), Some(attempt));
    assert_eq!(registry.pending_len(), 1);

    let expired = registry.expire(now + PENDING_CONNECTION_TIMEOUT_MS);

    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].attempt, attempt);
    assert_eq!(expired[0].age_ms, PENDING_CONNECTION_TIMEOUT_MS);
    assert_eq!(expired[0].phase.as_str(), "admitting");
    assert!(!registry.contains(peer));
    assert_eq!(registry.pending_len(), 0);
    assert!(registry.reserve(replacement_peer, now).is_ok());
    Ok(())
}

#[test]
fn test_promotion_replaces_pending_with_active_in_one_state_slot() -> Result<()> {
    let mut registry = ConnectionLifecycleRegistry::new(LifecycleBounds::new(1, 8));
    let now = 1_000;
    let peer = SecretKey::random().address().into();
    let attempt = registry.reserve(peer, now)?;

    assert!(registry.activate_for_test(attempt));
    assert_eq!(registry.pending_len(), 0);
    assert_eq!(registry.pending_attempt(peer), None);
    assert_eq!(registry.active_attempt(peer), Some(attempt));
    assert!(matches!(
        registry.reserve(peer, now),
        Err(Error::AlreadyConnected)
    ));
    Ok(())
}

#[test]
fn test_terminal_send_marker_is_generation_scoped_and_survives_until_retirement() -> Result<()> {
    let mut registry = ConnectionLifecycleRegistry::new(LifecycleBounds::new(1, 8));
    let peer = SecretKey::random().address().into();
    let attempt = registry.reserve(peer, 1_000)?;
    assert!(registry.activate_for_test(attempt));

    assert!(registry.mark_send_terminal(attempt));
    assert_eq!(registry.active_attempt(peer), Some(attempt));
    assert_eq!(registry.sendable_attempt(peer), None);
    assert_eq!(registry.active_connections().attempt(peer), None);
    assert_eq!(registry.admitted_connections().attempt(peer), Some(attempt));
    assert!(registry.remove_active(attempt));

    let replacement = registry.reserve(peer, 2_000)?;
    assert!(registry.activate_for_test(replacement));
    assert_eq!(registry.sendable_attempt(peer), Some(replacement));
    assert!(!registry.mark_send_terminal(attempt));
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_terminal_send_generation_cannot_reenter_topology() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    let connection = transport
        .admitted_send_connection(peer)?
        .ok_or(Error::ConnectionNotFound)?;
    assert!(connection.mark_send_terminal()?);
    assert!(transport.is_send_terminal_attempt(attempt)?);

    assert_eq!(transport.notify_admitted_predecessor(peer)?, None);
    assert_ne!(*transport.dht.lock_predecessor()?, Some(peer));
    assert_eq!(
        transport.record_finger_candidate(peer, 1)?,
        FingerUpdateDisposition::Unroutable
    );
    assert_eq!(transport.dht.lock_finger()?.get(1), None);
    Ok(())
}

#[tokio::test]
async fn test_admitted_peer_cannot_be_replaced_by_a_pending_handshake() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;

    assert!(transport.activate_connection_for_test(attempt)?);
    assert!(matches!(
        transport.reserve_pending_connection(peer).await,
        Err(Error::AlreadyConnected)
    ));
    Ok(())
}

#[tokio::test]
async fn test_pending_promotion_is_atomic_under_lifecycle_lock() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    let promotion_entered = Arc::new(std::sync::Barrier::new(2));
    let release_promotion = Arc::new(std::sync::Barrier::new(2));
    let reservation_prepared = Arc::new(std::sync::Barrier::new(2));
    let release_reservation_commit = Arc::new(std::sync::Barrier::new(2));
    let (reservation_contended_tx, reservation_contended_rx) = std::sync::mpsc::sync_channel(0);
    let (reservation_done_tx, reservation_done_rx) = std::sync::mpsc::channel();
    let reservation_transport = Arc::clone(&transport);
    let reservation_prepared_thread = Arc::clone(&reservation_prepared);
    let release_reservation_commit_thread = Arc::clone(&release_reservation_commit);
    let reservation = std::thread::spawn(move || {
        let result = futures::executor::block_on(
            reservation_transport.reserve_pending_connection_with_observer_for_test(
                peer,
                || {
                    reservation_prepared_thread.wait();
                    release_reservation_commit_thread.wait();
                },
                || {
                    let _ = reservation_contended_tx.send(
                        reservation_transport
                            .connection_lifecycle
                            .is_held_for_test(),
                    );
                },
            ),
        );
        let _ = reservation_done_tx.send(());
        result
    });

    // The reservation has completed its asynchronous cleanup. Only then start promotion and
    // hold the shared boundary; releasing this barrier makes the reservation attempt its real
    // commit lock, rather than merely racing before cleanup.
    reservation_prepared.wait();
    let promotion_transport = Arc::clone(&transport);
    let promotion_entered_thread = Arc::clone(&promotion_entered);
    let release_promotion_thread = Arc::clone(&release_promotion);
    let promotion = std::thread::spawn(move || {
        promotion_transport.activate_connection_with_observer_for_test(attempt, |transport| {
            assert!(transport.connection_lifecycle.is_held_for_test());
            promotion_entered_thread.wait();
            release_promotion_thread.wait();
        })
    });
    promotion_entered.wait();
    release_reservation_commit.wait();
    assert!(reservation_contended_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .map_err(|error| Error::InvalidMessage(format!("reservation did not contend: {error}")))?);
    assert!(matches!(
        reservation_done_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));

    release_promotion.wait();
    assert!(promotion
        .join()
        .map_err(|_| Error::InvalidMessage("promotion thread panicked".to_string()))??);
    assert!(matches!(
        reservation
            .join()
            .map_err(|_| Error::InvalidMessage("reservation thread panicked".to_string()))?,
        Err(Error::AlreadyConnected)
    ));
    assert!(transport.is_admitted_connection_attempt(attempt));
    Ok(())
}

#[tokio::test]
async fn test_admitting_peer_remains_unroutable_until_commit() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    let mut observed_transition = false;

    assert!(
        transport.begin_connection_admission_with_observer_for_test(attempt, |transport| {
            observed_transition = true;
            assert!(transport.connection_lifecycle.is_held_for_test());
            assert!(transport
                .is_admitting_connection_attempt(attempt)
                .expect("lifecycle registry must remain readable"));
            assert!(!transport.is_admitted_connection_attempt(attempt));
            assert!(!transport
                .dht
                .successors()
                .contains(&peer)
                .expect("successor state must remain readable"));
        },)?
    );

    assert!(observed_transition);
    assert!(transport.is_admitting_connection_attempt(attempt)?);
    assert!(transport.get_connection(peer).is_none());
    assert!(!transport.dht.successors().contains(&peer)?);
    assert_eq!(transport.pending_connection_count()?, 1);
    assert!(transport.cancel_pending_connection(attempt).await?);
    assert_eq!(transport.pending_connection_count()?, 0);
    Ok(())
}

#[tokio::test]
async fn test_pending_offer_is_not_routable_or_visible_to_dht() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));

    let _offer = transport.prepare_connection_offer(peer, callback).await?;

    assert!(transport.get_connection(peer).is_none());
    assert_eq!(transport.pending_connection_count()?, 1);
    assert!(!transport.dht.successors().contains(&peer)?);

    transport.disconnect(peer).await?;
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_incoming_offer_replaces_an_unroutable_admitted_generation() -> Result<()> {
    let local = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer = Arc::new(transport_with_key_and_measure(
        &peer_key,
        Arc::new(RecordingMeasure::default()),
    )?);
    let peer_did = peer.dht.did;

    let old_callback = InnerSwarmCallback::new(Arc::clone(&local), Arc::new(NoopSwarmCallback));
    let (old, _) = local
        .prepare_connection_offer_with_attempt(peer_did, old_callback)
        .await?;
    assert!(local.activate_connection_for_test(old)?);
    local.dht.join(peer_did)?;
    local.force_peer_connection_state_without_callback(
        peer_did,
        WebrtcConnectionState::Disconnected,
    )?;
    local.force_peer_data_channel_open_without_callback(peer_did, Some(true))?;

    let offer_callback = InnerSwarmCallback::new(Arc::clone(&peer), Arc::new(NoopSwarmCallback));
    let (_, offer) = peer
        .prepare_connection_offer_with_attempt(local.dht.did, offer_callback)
        .await?;
    let answer_callback = InnerSwarmCallback::new(Arc::clone(&local), Arc::new(NoopSwarmCallback));

    local
        .answer_remote_connection(peer_did, answer_callback, &offer)
        .await?;

    let replacement = local
        .pending_attempt(peer_did)?
        .ok_or(Error::SwarmMissTransport(peer_did))?;
    assert_ne!(replacement, old);
    assert!(!local.is_admitted_connection_attempt(old));
    assert!(
        !local.dht.successors().contains(&peer_did)?,
        "the retired generation must leave topology before replacement admission"
    );

    assert!(local.cancel_pending_connection(replacement).await?);
    peer.disconnect(local.dht.did).await?;
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_incoming_offer_replaces_an_orphaned_physical_connection() -> Result<()> {
    let local = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer = Arc::new(transport_with_key_and_measure(
        &peer_key,
        Arc::new(RecordingMeasure::default()),
    )?);
    let peer_did = peer.dht.did;

    let orphan_callback = InnerSwarmCallback::new(Arc::clone(&local), Arc::new(NoopSwarmCallback));
    let (orphan, _) = local
        .prepare_connection_offer_with_attempt(peer_did, orphan_callback)
        .await?;
    assert!(local.retire_pending_connection(orphan)?);
    assert_eq!(
        local.raw_connection_owner(peer_did)?,
        RawConnectionOwner::Orphan
    );

    let offer_callback = InnerSwarmCallback::new(Arc::clone(&peer), Arc::new(NoopSwarmCallback));
    let (_, offer) = peer
        .prepare_connection_offer_with_attempt(local.dht.did, offer_callback)
        .await?;
    let answer_callback = InnerSwarmCallback::new(Arc::clone(&local), Arc::new(NoopSwarmCallback));
    local
        .answer_remote_connection(peer_did, answer_callback, &offer)
        .await?;

    let replacement = local
        .pending_attempt(peer_did)?
        .ok_or(Error::SwarmMissTransport(peer_did))?;
    assert_ne!(replacement, orphan);
    assert_eq!(
        local.raw_connection_owner(peer_did)?,
        RawConnectionOwner::Pending(replacement)
    );

    assert!(local.cancel_pending_connection(replacement).await?);
    peer.disconnect(local.dht.did).await?;
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_physical_connection_creation_serializes_replaced_generations() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let creation = transport.connection_creation.lock(peer);
    let creation_guard = creation.lock().await;

    let old = transport.reserve_pending_connection(peer).await?;
    let old_callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback))
        .with_pending_connection_attempt(old);
    let old_creation = transport.new_pending_connection(old, old_callback);
    futures::pin_mut!(old_creation);
    assert!(futures::poll!(old_creation.as_mut()).is_pending());

    assert!(transport.retire_pending_connection(old)?);
    let replacement = transport.reserve_pending_connection(peer).await?;
    let replacement_callback =
        InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback))
            .with_pending_connection_attempt(replacement);
    let replacement_creation = transport.new_pending_connection(replacement, replacement_callback);
    futures::pin_mut!(replacement_creation);
    assert!(futures::poll!(replacement_creation.as_mut()).is_pending());

    drop(creation_guard);
    assert!(matches!(
        old_creation.await,
        Err(Error::ConnectionAttemptSuperseded {
            peer: observed,
            generation,
        }) if observed == peer && generation == old.generation()
    ));
    replacement_creation.await?;

    assert!(transport.is_pending_connection_attempt(replacement)?);
    assert!(transport.get_raw_connection(peer).is_some());
    assert!(transport.cancel_pending_connection(replacement).await?);
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_final_send_admission_serializes_generation_route_and_readiness() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    transport.dht.join(peer)?;

    let admitted = transport
        .admitted_send_connection(peer)?
        .ok_or(Error::SwarmMissTransport(peer))?;
    let admission = admitted;
    let dht = Arc::clone(&transport.dht);
    let send_permit = SendPermit::always();
    let acceptance = send_permit.acceptance();
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let admission_thread = {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        std::thread::spawn(move || -> Result<bool> {
            let route_check = admission.with_current_connection(|connection| {
                dht.with_permitted_storage_sync_route(
                    StorageSyncDestination::PhysicalOwner(peer),
                    peer,
                    || {
                        entered.wait();
                        release.wait();
                        connection.readiness().can_make_progress()
                            && send_permit.try_mark_irrevocable().is_some()
                    },
                )
            })?;
            let Some(route_check) = route_check else {
                return Err(Error::ConnectionAttemptSuperseded {
                    peer,
                    generation: attempt.generation(),
                });
            };
            route_check?.ok_or_else(|| {
                Error::InvalidMessage("storage route was unexpectedly revoked".to_string())
            })
        })
    };
    entered.wait();

    let (retirement_started_tx, retirement_started_rx) = std::sync::mpsc::sync_channel(0);
    let (retirement_done_tx, retirement_done_rx) = std::sync::mpsc::channel();
    let retirement_transport = Arc::clone(&transport);
    let retirement_thread = std::thread::spawn(move || {
        let _ = retirement_started_tx.send(());
        let result = retirement_transport.retire_active_connection_with(attempt, |_| Ok(()));
        let _ = retirement_done_tx.send(());
        result
    });
    retirement_started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .map_err(|error| Error::InvalidMessage(format!("retirement did not start: {error}")))?;

    let (topology_started_tx, topology_started_rx) = std::sync::mpsc::sync_channel(0);
    let (topology_done_tx, topology_done_rx) = std::sync::mpsc::channel();
    let topology_dht = Arc::clone(&transport.dht);
    let topology_thread = std::thread::spawn(move || {
        let _ = topology_started_tx.send(());
        let result = topology_dht.remove(peer);
        let _ = topology_done_tx.send(());
        result
    });
    topology_started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .map_err(|error| {
            Error::InvalidMessage(format!("topology update did not start: {error}"))
        })?;

    assert!(retirement_done_rx
        .recv_timeout(std::time::Duration::from_millis(25))
        .is_err());
    assert!(topology_done_rx
        .recv_timeout(std::time::Duration::from_millis(25))
        .is_err());

    release.wait();
    assert!(admission_thread
        .join()
        .map_err(|_| Error::InvalidMessage("send admission thread panicked".to_string()))??);
    assert!(acceptance.is_irrevocable());
    assert_eq!(
        retirement_thread
            .join()
            .map_err(|_| Error::InvalidMessage("retirement thread panicked".to_string()))??,
        Some(())
    );
    topology_thread
        .join()
        .map_err(|_| Error::InvalidMessage("topology thread panicked".to_string()))??;

    assert!(!transport.is_admitted_connection(peer));
    assert!(!transport.dht.successors().contains(&peer)?);
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_stale_send_after_retirement_does_not_recreate_outbound_scheduler() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    let payload = MessagePayload::new_send(
        Message::custom(b"stale-after-admission")?,
        transport.message_signer(),
        peer,
        peer,
    )?;
    let retire_transport = Arc::clone(&transport);

    let outcome = transport
        .send_payload_detached_observing_scheduler_submit_for_test(payload, move || {
            assert!(matches!(
                retire_transport.retire_active_connection_with(attempt, |_| Ok(())),
                Ok(Some(()))
            ));
        })
        .await?;

    assert_eq!(outcome, SendCompletionOutcome::Cancelled);
    assert_eq!(transport.outbound_schedulers.peer_count_for_test(), 0);
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_scheduler_shutdown_revokes_a_frame_waiting_at_transport_dispatch() -> Result<()> {
    let (transport, peer, _attempt) = transport_with_routable_peer().await?;
    let payload = MessagePayload::new_send(
        Message::custom(b"shutdown-at-dispatch")?,
        transport.message_signer(),
        peer,
        peer,
    )?;
    dummy_controlled::reset_sent_count();
    dummy_controlled::pause_send_message_at_dispatch();
    let sending_transport = Arc::clone(&transport);
    let send = tokio::spawn(async move {
        sending_transport
            .send_payload_detached_with_outcome(payload)
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !dummy_controlled::send_message_waiting_at_dispatch() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| Error::InvalidMessage("send did not reach dispatch gate".to_string()))?;
    transport.outbound_schedulers.shutdown(peer);
    dummy_controlled::release_send_message_gate();

    let outcome = send
        .await
        .map_err(|error| Error::InvalidMessage(format!("send task failed: {error}")))??;
    assert_eq!(outcome, SendCompletionOutcome::Cancelled);
    assert_eq!(dummy_controlled::sent_count(), 0);
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_scheduler_shutdown_after_backend_acceptance_preserves_detached_success() -> Result<()>
{
    let (transport, peer, _attempt) = transport_with_routable_peer().await?;
    let payload = MessagePayload::new_send(
        Message::custom(b"shutdown-after-backend-acceptance")?,
        transport.message_signer(),
        peer,
        peer,
    )?;
    dummy_controlled::reset_sent_count();
    dummy_controlled::set_drop_messages(true);
    dummy_controlled::pause_irrevocable_send();
    let sending_transport = Arc::clone(&transport);
    let send = tokio::spawn(async move {
        sending_transport
            .send_payload_detached_with_outcome(payload)
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !dummy_controlled::irrevocable_send_gate_waiting() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| Error::InvalidMessage("send did not become irrevocable".to_string()))?;
    transport.outbound_schedulers.shutdown(peer);
    dummy_controlled::release_irrevocable_send_gate();

    let outcome = send
        .await
        .map_err(|error| Error::InvalidMessage(format!("send task failed: {error}")))??;
    dummy_controlled::set_drop_messages(false);
    assert_eq!(outcome, SendCompletionOutcome::Succeeded);
    assert_eq!(dummy_controlled::sent_count(), 1);
    assert_eq!(
        transport.outbound_admitted_transfer_total_for_test(),
        0,
        "accepted completion must publish only after scheduler capacity is released"
    );
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_scheduler_shutdown_cancels_a_send_before_queue_acceptance() -> Result<()> {
    let (transport, peer, _attempt) = transport_with_routable_peer().await?;
    let payload = MessagePayload::new_send(
        Message::custom(b"linearized-before-shutdown")?,
        transport.message_signer(),
        peer,
        peer,
    )?;
    dummy_controlled::reset_sent_count();
    dummy_controlled::set_drop_messages(true);
    dummy_controlled::pause_send_message_after_permit();
    let sending_transport = Arc::clone(&transport);
    let send = tokio::spawn(async move {
        sending_transport
            .send_payload_detached_with_outcome(payload)
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !dummy_controlled::post_permit_send_gate_waiting() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| Error::InvalidMessage("send did not reach its acceptance gate".to_string()))?;
    transport.outbound_schedulers.shutdown(peer);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), send)
        .await
        .map_err(|_| Error::InvalidMessage("unaccepted send did not cancel".to_string()))?
        .map_err(|error| Error::InvalidMessage(format!("send task failed: {error}")))??;
    dummy_controlled::release_post_permit_send_gate();
    dummy_controlled::set_drop_messages(false);
    assert_eq!(outcome, SendCompletionOutcome::Cancelled);
    assert_eq!(dummy_controlled::sent_count(), 0);
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_routable_join_serializes_with_generation_retirement() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    let (hold_lifecycle, lifecycle_gate) = lifecycle_test_gate();
    let join_transport = Arc::clone(&transport);
    let join_thread = BoundedThread::spawn(move || {
        join_transport.join_routable_peer_with_observer_for_test(peer, hold_lifecycle)
    });
    lifecycle_gate.wait_until_entered()?;
    let retirement = BlockedRetirement::spawn(Arc::clone(&transport), peer, attempt);
    retirement.wait_until_registered(&transport)?;

    lifecycle_gate.release()?;
    assert!(join_thread.finish("routable join")??.is_some());
    assert_eq!(retirement.finish()?, Some(()));
    assert!(!transport.is_admitted_connection(peer));
    assert!(!transport.dht.successors().contains(&peer)?);
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_topology_report_serializes_with_generation_retirement() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    let reported = TopoInfo {
        successors: vec![peer],
        predecessor: Some(peer),
    };
    let (hold_lifecycle, lifecycle_gate) = lifecycle_test_gate();
    let stabilization_transport = Arc::clone(&transport);
    let stabilization_thread = BoundedThread::spawn(move || {
        stabilization_transport
            .stabilize_routable_topology_with_observer_for_test(&reported, hold_lifecycle)
    });
    lifecycle_gate.wait_until_entered()?;
    let retirement = BlockedRetirement::spawn(Arc::clone(&transport), peer, attempt);
    retirement.wait_until_registered(&transport)?;

    lifecycle_gate.release()?;
    assert!(stabilization_thread.finish("topology report")??.is_some());
    assert_eq!(retirement.finish()?, Some(()));
    assert!(!transport.is_admitted_connection(peer));
    assert!(!transport.dht.successors().contains(&peer)?);
    assert_ne!(*transport.dht.lock_predecessor()?, Some(peer));
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_predecessor_notification_serializes_with_generation_retirement() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    let (hold_lifecycle, lifecycle_gate) = lifecycle_test_gate();
    let notification_transport = Arc::clone(&transport);
    let notification_thread = BoundedThread::spawn(move || {
        notification_transport
            .notify_admitted_predecessor_with_observer_for_test(peer, hold_lifecycle)
    });
    lifecycle_gate.wait_until_entered()?;
    let retirement = BlockedRetirement::spawn(Arc::clone(&transport), peer, attempt);
    retirement.wait_until_registered(&transport)?;

    lifecycle_gate.release()?;
    assert_eq!(
        notification_thread.finish("predecessor notification")??,
        Some(peer)
    );
    assert_eq!(retirement.finish()?, Some(()));
    assert!(!transport.is_admitted_connection(peer));
    assert_ne!(*transport.dht.lock_predecessor()?, Some(peer));
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_finger_update_serializes_with_generation_retirement() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    let finger_index = 3;
    let (hold_lifecycle, lifecycle_gate) = lifecycle_test_gate();
    let finger_transport = Arc::clone(&transport);
    let finger_thread = BoundedThread::spawn(move || {
        finger_transport.record_finger_candidate_with_observer_for_test(
            peer,
            finger_index,
            hold_lifecycle,
        )
    });
    lifecycle_gate.wait_until_entered()?;
    let retirement = BlockedRetirement::spawn(Arc::clone(&transport), peer, attempt);
    retirement.wait_until_registered(&transport)?;

    lifecycle_gate.release()?;
    assert_eq!(
        finger_thread.finish("finger update")??,
        FingerUpdateDisposition::Applied
    );
    assert_eq!(retirement.finish()?, Some(()));
    assert!(!transport.is_admitted_connection(peer));
    assert!(!transport.dht.lock_finger()?.contains(Some(peer)));
    Ok(())
}
