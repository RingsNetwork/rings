use super::pending::LifecycleBounds;
use super::pending::ReservationVerdict;
use super::*;
use crate::dht::Chord;
use crate::utils::get_epoch_ms_i64;

fn bounded_transport(pending: usize, total: usize) -> Result<SwarmTransport> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    transport.set_lifecycle_bounds_for_test(LifecycleBounds::new(pending, total))?;
    Ok(transport)
}

/// Admit a peer whose liveness record is `age_ms` old and whose last inbound
/// payload is `idle_ms` old.
async fn admit_peer(
    transport: &SwarmTransport,
    age_ms: i64,
    idle_ms: i64,
) -> Result<PendingConnectionAttempt> {
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    transport.mark_peer_liveness_connected(attempt);
    let now_ms = get_epoch_ms_i64();
    transport.force_peer_connected_at(peer, now_ms - age_ms)?;
    transport.force_peer_last_inbound_at(peer, now_ms - idle_ms)?;
    Ok(attempt)
}

async fn admit_aged_peer(
    transport: &SwarmTransport,
    age_ms: i64,
) -> Result<PendingConnectionAttempt> {
    admit_peer(transport, age_ms, age_ms).await
}

#[tokio::test]
async fn test_full_registry_rejects_reservation_while_every_peer_is_within_grace() -> Result<()> {
    let transport = bounded_transport(2, 2)?;
    let _first = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS / 2).await?;
    let _second = admit_aged_peer(&transport, 0).await?;
    let newcomer = SecretKey::random().address().into();

    let result = transport.reserve_pending_connection(newcomer).await;

    assert!(matches!(
        result,
        Err(Error::ConnectionCapacityExceeded { capacity: 2 })
    ));
    assert_eq!(transport.admitted_connection_ids().len(), 2);
    Ok(())
}

#[tokio::test]
async fn test_full_registry_evicts_most_idle_unreferenced_peer_for_a_newcomer() -> Result<()> {
    let transport = bounded_transport(2, 2)?;
    let grace = UNREFERENCED_CONNECTION_GRACE_MS;
    let older_but_active = admit_peer(&transport, grace * 4, 1_000).await?;
    let younger_but_idle = admit_peer(&transport, grace, grace * 2).await?;
    let newcomer = SecretKey::random().address().into();

    let attempt = transport.reserve_pending_connection(newcomer).await?;

    assert!(transport.is_admitted_connection_attempt(older_but_active));
    assert!(!transport.is_admitted_connection(younger_but_idle.peer));
    assert!(transport.is_pending_connection_attempt(attempt)?);
    assert_eq!(transport.pending_connection_count()?, 1);
    Ok(())
}

#[tokio::test]
async fn test_full_registry_evicts_send_terminal_peer_before_any_live_peer() -> Result<()> {
    let transport = bounded_transport(2, 2)?;
    let grace = UNREFERENCED_CONNECTION_GRACE_MS;
    let idle_live = admit_peer(&transport, grace * 4, grace * 4).await?;
    let dead_young = admit_peer(&transport, 0, 0).await?;
    assert!(transport.peer_lifecycles()?.mark_send_terminal(dead_young));
    let newcomer = SecretKey::random().address().into();

    transport.reserve_pending_connection(newcomer).await?;

    assert!(transport.is_admitted_connection_attempt(idle_live));
    assert!(!transport.is_admitted_connection(dead_young.peer));
    Ok(())
}

#[tokio::test]
async fn test_full_registry_keeps_topology_referenced_peer_over_idler_unreferenced_peer(
) -> Result<()> {
    let transport = bounded_transport(2, 2)?;
    let grace = UNREFERENCED_CONNECTION_GRACE_MS;
    let referenced = admit_peer(&transport, grace * 2, grace * 4).await?;
    transport.dht.join(referenced.peer)?;
    let unreferenced = admit_peer(&transport, grace, grace).await?;
    let newcomer = SecretKey::random().address().into();

    transport.reserve_pending_connection(newcomer).await?;

    assert!(transport.is_admitted_connection_attempt(referenced));
    assert!(transport.dht.successors().contains(&referenced.peer)?);
    assert!(!transport.is_admitted_connection(unreferenced.peer));
    Ok(())
}

#[tokio::test]
async fn test_full_registry_does_not_evict_for_a_peer_that_already_owns_a_record() -> Result<()> {
    let transport = bounded_transport(2, 2)?;
    let older = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS * 2).await?;
    let old = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS).await?;

    assert_eq!(
        transport.reservation_verdict_for_test(old.peer)?,
        ReservationVerdict::AlreadyConnected
    );
    let result = transport.reserve_pending_connection(old.peer).await;

    assert!(matches!(result, Err(Error::AlreadyConnected)));
    assert!(transport.is_admitted_connection_attempt(older));
    assert!(transport.is_admitted_connection_attempt(old));
    Ok(())
}

/// A saturated handshake set cannot be relieved by retiring an admitted
/// record, so no eviction is spent on a reservation that fails anyway.
#[tokio::test]
async fn test_pending_saturated_registry_rejects_without_evicting() -> Result<()> {
    let transport = bounded_transport(1, 3)?;
    let admitted = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS * 2).await?;
    let handshaking = SecretKey::random().address().into();
    let _pending = transport.reserve_pending_connection(handshaking).await?;
    let newcomer = SecretKey::random().address().into();

    assert_eq!(
        transport.reservation_verdict_for_test(newcomer)?,
        ReservationVerdict::PendingCapacityExceeded
    );
    let result = transport.reserve_pending_connection(newcomer).await;

    assert!(matches!(
        result,
        Err(Error::PendingConnectionCapacityExceeded { capacity: 1 })
    ));
    assert!(transport.is_admitted_connection_attempt(admitted));
    Ok(())
}

/// A candidate that a topology transition references after the plan was
/// taken is rejected inside the retirement critical section and skipped for
/// the next one.
#[tokio::test]
async fn test_eviction_skips_candidate_referenced_after_the_plan() -> Result<()> {
    let transport = bounded_transport(2, 2)?;
    let grace = UNREFERENCED_CONNECTION_GRACE_MS;
    let most_idle = admit_peer(&transport, grace * 2, grace * 2).await?;
    let less_idle = admit_peer(&transport, grace, grace).await?;

    transport
        .evict_unreferenced_connection_with_plan_observer_for_test(get_epoch_ms_i64(), |plan| {
            assert_eq!(plan, [most_idle, less_idle]);
            transport
                .dht
                .join(most_idle.peer)
                .expect("test join must succeed");
        })
        .await?;

    assert!(transport.is_admitted_connection_attempt(most_idle));
    assert!(!transport.is_admitted_connection(less_idle.peer));
    Ok(())
}

/// A candidate whose generation was superseded after the plan cannot retire
/// the replacement; the next candidate is evicted instead.
#[tokio::test]
async fn test_eviction_skips_candidate_superseded_after_the_plan() -> Result<()> {
    let transport = bounded_transport(2, 2)?;
    let grace = UNREFERENCED_CONNECTION_GRACE_MS;
    let most_idle = admit_peer(&transport, grace * 2, grace * 2).await?;
    let less_idle = admit_peer(&transport, grace, grace).await?;

    let mut replacement = None;
    transport
        .evict_unreferenced_connection_with_plan_observer_for_test(get_epoch_ms_i64(), |plan| {
            assert_eq!(plan, [most_idle, less_idle]);
            let (_, replaced) = transport
                .replace_active_generation_for_test(most_idle.peer)
                .expect("test generation replacement must succeed");
            replacement = Some(replaced);
        })
        .await?;

    let replacement = replacement.ok_or(Error::SwarmMissTransport(most_idle.peer))?;
    assert!(transport.is_admitted_connection_attempt(replacement));
    assert!(!transport.is_admitted_connection(less_idle.peer));
    Ok(())
}
