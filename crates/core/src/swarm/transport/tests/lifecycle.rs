use super::super::pending::LifecycleTransitionGate;
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

#[test]
fn connection_lifecycle_registry_is_bounded_and_rejects_duplicate_peers() -> Result<()> {
    let mut registry = ConnectionLifecycleRegistry::<2>::new();
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
fn stale_pending_callback_cannot_remove_a_replacement_attempt() -> Result<()> {
    let mut registry = ConnectionLifecycleRegistry::<1>::new();
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
fn connection_lifecycle_registry_expires_only_unopened_handshakes() -> Result<()> {
    let mut registry = ConnectionLifecycleRegistry::<1>::new();
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
fn connection_lifecycle_registry_expires_abandoned_admission() -> Result<()> {
    let mut registry = ConnectionLifecycleRegistry::<1>::new();
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
fn promotion_replaces_pending_with_active_in_one_state_slot() -> Result<()> {
    let mut registry = ConnectionLifecycleRegistry::<1>::new();
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

#[tokio::test]
async fn admitted_peer_cannot_be_replaced_by_a_pending_handshake() -> Result<()> {
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
async fn pending_promotion_is_atomic_under_lifecycle_lock() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    let mut observed_transition = false;

    assert!(
        transport.activate_connection_with_observer_for_test(attempt, |transport| {
            observed_transition = true;
            assert!(transport.connection_lifecycle.is_held_for_test());
        },)?
    );

    assert!(observed_transition);
    assert!(transport.is_admitted_connection_attempt(attempt));
    assert!(matches!(
        transport.reserve_pending_connection(peer).await,
        Err(Error::AlreadyConnected)
    ));
    Ok(())
}

#[tokio::test]
async fn pending_promotion_excludes_a_concurrent_reservation_until_commit() -> Result<()> {
    // Invariant: a promotion holding the lifecycle boundary excludes a competing
    // reservation; after commit, the same peer has exactly one active generation.
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    let promotion_gate = LifecycleTransitionGate::new();
    let promotion = Arc::clone(&transport)
        .activate_connection_with_gate_for_test(attempt, promotion_gate.clone());

    promotion_gate.wait_until_entered().await;

    let reservation_started = Arc::new(tokio::sync::Notify::new());
    let reservation = {
        let reservation_transport = Arc::clone(&transport);
        let reservation_started = Arc::clone(&reservation_started);
        tokio::task::spawn_blocking(move || {
            futures::executor::block_on(
                reservation_transport.reserve_pending_connection_with_observer_for_test(
                    peer,
                    || {
                        reservation_started.notify_one();
                    },
                ),
            )
        })
    };
    reservation_started.notified().await;
    assert!(!reservation.is_finished());

    promotion_gate.release();
    assert!(promotion
        .await
        .map_err(|error| Error::InvalidMessage(format!("promotion worker failed: {error}")))??);
    assert!(matches!(
        reservation
            .await
            .map_err(|error| Error::InvalidMessage(format!(
                "reservation worker failed: {error}"
            )))?,
        Err(Error::AlreadyConnected)
    ));
    assert!(transport.is_admitted_connection_attempt(attempt));
    Ok(())
}

#[tokio::test]
async fn admitting_peer_remains_unroutable_until_commit() -> Result<()> {
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
async fn pending_offer_is_not_routable_or_visible_to_dht() -> Result<()> {
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
async fn incoming_offer_replaces_an_unroutable_admitted_generation() -> Result<()> {
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
async fn incoming_offer_replaces_an_orphaned_physical_connection() -> Result<()> {
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
async fn physical_connection_creation_serializes_replaced_generations() -> Result<()> {
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
async fn final_send_admission_serializes_generation_route_and_readiness() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    transport.dht.join(peer)?;

    let admitted = transport
        .admitted_send_connection(peer)?
        .ok_or(Error::SwarmMissTransport(peer))?;
    let admission = admitted;
    let dht = Arc::clone(&transport.dht);
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let admission_thread = {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        std::thread::spawn(move || -> Result<bool> {
            let route_check = admission.with_current(|connection| {
                dht.with_permitted_storage_sync_route(
                    StorageSyncDestination::PhysicalOwner(peer),
                    peer,
                    || {
                        entered.wait();
                        release.wait();
                        connection.readiness().can_make_progress()
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
async fn routable_join_serializes_with_generation_retirement() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let join_transport = Arc::clone(&transport);
    let join_thread = {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        std::thread::spawn(move || {
            join_transport.join_routable_peer_with_observer_for_test(peer, || {
                entered.wait();
                release.wait();
            })
        })
    };
    entered.wait();

    let (retirement_started_tx, retirement_started_rx) = std::sync::mpsc::sync_channel(0);
    let (retirement_done_tx, retirement_done_rx) = std::sync::mpsc::channel();
    let retirement_transport = Arc::clone(&transport);
    let retirement_thread = std::thread::spawn(move || {
        let _ = retirement_started_tx.send(());
        let result = retirement_transport.retire_active_connection_with(attempt, |_| {
            retirement_transport.dht.remove(peer)?;
            Ok(())
        });
        let _ = retirement_done_tx.send(());
        result
    });
    retirement_started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .map_err(|error| Error::InvalidMessage(format!("retirement did not start: {error}")))?;
    assert!(retirement_done_rx
        .recv_timeout(std::time::Duration::from_millis(25))
        .is_err());

    release.wait();
    assert!(join_thread
        .join()
        .map_err(|_| Error::InvalidMessage("join thread panicked".to_string()))??
        .is_some());
    assert_eq!(
        retirement_thread
            .join()
            .map_err(|_| Error::InvalidMessage("retirement thread panicked".to_string()))??,
        Some(())
    );
    assert!(!transport.is_admitted_connection(peer));
    assert!(!transport.dht.successors().contains(&peer)?);
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn topology_report_serializes_with_generation_retirement() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    let reported = TopoInfo {
        successors: vec![peer],
        predecessor: Some(peer),
    };
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let stabilization_transport = Arc::clone(&transport);
    let stabilization_thread = {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        std::thread::spawn(move || {
            stabilization_transport.stabilize_routable_topology_with_observer_for_test(
                &reported,
                || {
                    entered.wait();
                    release.wait();
                },
            )
        })
    };
    entered.wait();

    let (retirement_started_tx, retirement_started_rx) = std::sync::mpsc::sync_channel(0);
    let (retirement_done_tx, retirement_done_rx) = std::sync::mpsc::channel();
    let retirement_transport = Arc::clone(&transport);
    let retirement_thread = std::thread::spawn(move || {
        let _ = retirement_started_tx.send(());
        let result = retirement_transport.retire_active_connection_with(attempt, |_| {
            retirement_transport.dht.remove(peer)?;
            Ok(())
        });
        let _ = retirement_done_tx.send(());
        result
    });
    retirement_started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .map_err(|error| Error::InvalidMessage(format!("retirement did not start: {error}")))?;
    assert!(retirement_done_rx
        .recv_timeout(std::time::Duration::from_millis(25))
        .is_err());

    release.wait();
    assert!(stabilization_thread
        .join()
        .map_err(|_| Error::InvalidMessage("stabilization thread panicked".to_string()))??
        .is_some());
    assert_eq!(
        retirement_thread
            .join()
            .map_err(|_| Error::InvalidMessage("retirement thread panicked".to_string()))??,
        Some(())
    );
    assert!(!transport.is_admitted_connection(peer));
    assert!(!transport.dht.successors().contains(&peer)?);
    assert_ne!(*transport.dht.lock_predecessor()?, Some(peer));
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn predecessor_notification_serializes_with_generation_retirement() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let notification_transport = Arc::clone(&transport);
    let notification_thread = {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        std::thread::spawn(move || {
            notification_transport.notify_admitted_predecessor_with_observer_for_test(peer, || {
                entered.wait();
                release.wait();
            })
        })
    };
    entered.wait();

    let (retirement_started_tx, retirement_started_rx) = std::sync::mpsc::sync_channel(0);
    let (retirement_done_tx, retirement_done_rx) = std::sync::mpsc::channel();
    let retirement_transport = Arc::clone(&transport);
    let retirement_thread = std::thread::spawn(move || {
        let _ = retirement_started_tx.send(());
        let result = retirement_transport.retire_active_connection_with(attempt, |_| {
            retirement_transport.dht.remove(peer)?;
            Ok(())
        });
        let _ = retirement_done_tx.send(());
        result
    });
    retirement_started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .map_err(|error| Error::InvalidMessage(format!("retirement did not start: {error}")))?;
    assert!(retirement_done_rx
        .recv_timeout(std::time::Duration::from_millis(25))
        .is_err());

    release.wait();
    assert_eq!(
        notification_thread
            .join()
            .map_err(|_| Error::InvalidMessage("notification thread panicked".to_string()))??,
        Some(peer)
    );
    assert_eq!(
        retirement_thread
            .join()
            .map_err(|_| Error::InvalidMessage("retirement thread panicked".to_string()))??,
        Some(())
    );
    assert!(!transport.is_admitted_connection(peer));
    assert_ne!(*transport.dht.lock_predecessor()?, Some(peer));
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn finger_update_serializes_with_generation_retirement() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    let finger_index = 3;
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let finger_transport = Arc::clone(&transport);
    let finger_thread = {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        std::thread::spawn(move || {
            finger_transport.record_finger_candidate_with_observer_for_test(
                peer,
                finger_index,
                || {
                    entered.wait();
                    release.wait();
                },
            )
        })
    };
    entered.wait();

    let (retirement_started_tx, retirement_started_rx) = std::sync::mpsc::sync_channel(0);
    let (retirement_done_tx, retirement_done_rx) = std::sync::mpsc::channel();
    let retirement_transport = Arc::clone(&transport);
    let retirement_thread = std::thread::spawn(move || {
        let _ = retirement_started_tx.send(());
        let result = retirement_transport.retire_active_connection_with(attempt, |_| {
            retirement_transport.dht.remove(peer)?;
            Ok(())
        });
        let _ = retirement_done_tx.send(());
        result
    });
    retirement_started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .map_err(|error| Error::InvalidMessage(format!("retirement did not start: {error}")))?;
    assert!(retirement_done_rx
        .recv_timeout(std::time::Duration::from_millis(25))
        .is_err());

    release.wait();
    assert_eq!(
        finger_thread
            .join()
            .map_err(|_| Error::InvalidMessage("finger update thread panicked".to_string()))??,
        FingerUpdateDisposition::Applied
    );
    assert_eq!(
        retirement_thread
            .join()
            .map_err(|_| Error::InvalidMessage("retirement thread panicked".to_string()))??,
        Some(())
    );
    assert!(!transport.is_admitted_connection(peer));
    assert!(!transport.dht.lock_finger()?.contains(Some(peer)));
    Ok(())
}
