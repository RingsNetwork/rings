use super::pending::LifecycleBounds;
use super::*;
use crate::dht::Chord;
use crate::utils::get_epoch_ms_i64;

fn bounded_transport(total: usize) -> Result<SwarmTransport> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    transport.set_lifecycle_bounds_for_test(LifecycleBounds { pending: 8, total })?;
    Ok(transport)
}

async fn admit_aged_peer(
    transport: &SwarmTransport,
    age_ms: i64,
) -> Result<PendingConnectionAttempt> {
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    transport.mark_peer_liveness_connected(attempt);
    transport.force_peer_connected_at(peer, get_epoch_ms_i64() - age_ms)?;
    Ok(attempt)
}

#[tokio::test]
async fn test_full_registry_rejects_reservation_while_every_peer_is_within_grace() -> Result<()> {
    let transport = bounded_transport(2)?;
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
async fn test_full_registry_evicts_oldest_unreferenced_peer_for_a_newcomer() -> Result<()> {
    let transport = bounded_transport(2)?;
    let older = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS * 2).await?;
    let old = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS).await?;
    let newcomer = SecretKey::random().address().into();

    let attempt = transport.reserve_pending_connection(newcomer).await?;

    assert!(!transport.is_admitted_connection(older.peer));
    assert!(transport.is_admitted_connection_attempt(old));
    assert!(transport.is_pending_connection_attempt(attempt)?);
    assert_eq!(transport.pending_connection_count()?, 1);
    Ok(())
}

#[tokio::test]
async fn test_full_registry_keeps_topology_referenced_peer_over_younger_unreferenced_peer(
) -> Result<()> {
    let transport = bounded_transport(2)?;
    let referenced = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS * 2).await?;
    transport.dht.join(referenced.peer)?;
    let unreferenced = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS).await?;
    let newcomer = SecretKey::random().address().into();

    transport.reserve_pending_connection(newcomer).await?;

    assert!(transport.is_admitted_connection_attempt(referenced));
    assert!(transport.dht.successors().contains(&referenced.peer)?);
    assert!(!transport.is_admitted_connection(unreferenced.peer));
    Ok(())
}

#[tokio::test]
async fn test_full_registry_does_not_evict_for_a_peer_that_already_owns_a_record() -> Result<()> {
    let transport = bounded_transport(2)?;
    let older = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS * 2).await?;
    let old = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS).await?;

    assert!(!transport.reservation_needs_eviction(old.peer)?);
    let result = transport.reserve_pending_connection(old.peer).await;

    assert!(matches!(result, Err(Error::AlreadyConnected)));
    assert!(transport.is_admitted_connection_attempt(older));
    assert!(transport.is_admitted_connection_attempt(old));
    Ok(())
}

/// A candidate that a topology transition references after the plan was
/// taken is re-validated at retirement time and skipped for the next one.
#[tokio::test]
async fn test_eviction_skips_candidate_referenced_after_the_plan() -> Result<()> {
    let transport = bounded_transport(2)?;
    let older = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS * 2).await?;
    let old = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS).await?;

    transport
        .evict_unreferenced_connection_with_plan_observer_for_test(get_epoch_ms_i64(), |plan| {
            assert_eq!(plan, [older, old]);
            transport
                .dht
                .join(older.peer)
                .expect("test join must succeed");
        })
        .await?;

    assert!(transport.is_admitted_connection_attempt(older));
    assert!(!transport.is_admitted_connection(old.peer));
    Ok(())
}

/// A candidate whose generation was superseded after the plan cannot retire
/// the replacement; the next candidate is evicted instead.
#[tokio::test]
async fn test_eviction_skips_candidate_superseded_after_the_plan() -> Result<()> {
    let transport = bounded_transport(2)?;
    let older = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS * 2).await?;
    let old = admit_aged_peer(&transport, UNREFERENCED_CONNECTION_GRACE_MS).await?;

    let mut replacement = None;
    transport
        .evict_unreferenced_connection_with_plan_observer_for_test(get_epoch_ms_i64(), |plan| {
            assert_eq!(plan, [older, old]);
            let (_, replaced) = transport
                .replace_active_generation_for_test(older.peer)
                .expect("test generation replacement must succeed");
            replacement = Some(replaced);
        })
        .await?;

    let replacement = replacement.ok_or(Error::SwarmMissTransport(older.peer))?;
    assert!(transport.is_admitted_connection_attempt(replacement));
    assert!(!transport.is_admitted_connection(old.peer));
    Ok(())
}
