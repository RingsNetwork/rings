use super::*;
use crate::dht::Chord;
use crate::swarm::transport::ConnectionCapacity;
use crate::swarm::transport::UNREFERENCED_CONNECTION_GRACE_MS;
use crate::utils::get_epoch_ms_i64;

fn bounded_transport(capacity: usize) -> Result<SwarmTransport> {
    let session_sk = SessionSk::new_with_seckey(&SecretKey::random())?;
    let dht = Arc::new(PeerRing::new_with_storage_and_finger_table_size(
        session_sk.account_did(),
        3,
        Box::new(MemStorage::new()),
        DEFAULT_FINGER_TABLE_SIZE,
    ));
    Ok(SwarmTransport::new(
        0,
        SwarmWebrtcConfig::new("".to_string(), None, None),
        session_sk,
        dht,
        Some(Arc::new(RecordingMeasure::default())),
        SwarmTransportSettings::new(
            1,
            VirtualNodeConfig::disabled(),
            ReassemblyLimits::production(),
            ConnectionCapacity::exact_for_test(capacity),
        ),
    ))
}

async fn admit_aged_peer(
    transport: &SwarmTransport,
    connected_for_ms: i64,
) -> Result<PendingConnectionAttempt> {
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    transport.mark_peer_liveness_connected(attempt);
    transport.force_peer_connected_at(peer, get_epoch_ms_i64() - connected_for_ms)?;
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

    let result = transport.reserve_pending_connection(old.peer).await;

    assert!(matches!(result, Err(Error::AlreadyConnected)));
    assert!(transport.is_admitted_connection_attempt(older));
    assert!(transport.is_admitted_connection_attempt(old));
    Ok(())
}
