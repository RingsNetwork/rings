//! The retirement law: every connection generation is announced `PeerAdmitted` in the
//! transition that activates it and `PeerRetired` in the transition that retires it, and a
//! topology prune that keeps the record announces nothing.
//!
//! The inbox withdraws an unread `PeerAdmitted p` together with its `PeerRetired p`, so the
//! application never reads a departure without its admission: a test that observes both reads
//! the admission before the retirement is decided.

use super::pending::RetirementOutcome;
use super::*;
use crate::dht::topology::TopologyRemoval;
use crate::swarm::application::SwarmEvent;

/// A transport whose application inbox is claimed by a fresh recorder.
fn transport_with_log() -> Result<(SwarmTransport, ApplicationRecorder)> {
    let transport = TransportSpec::default().build()?;
    let log = ApplicationRecorder::attach(&transport)?;
    Ok((transport, log))
}

/// An admission is announced as the record activates, and its retirement is reported exactly
/// once; a second retirement of the same attempt is not one.
#[tokio::test]
async fn test_a_read_admission_is_reported_retired_once() -> Result<()> {
    let (transport, log) = transport_with_log()?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    assert_eq!(log.events(), vec![SwarmEvent::PeerAdmitted { peer }]);

    assert!(transport.disconnect_attempt(attempt).await?.is_some());
    assert_eq!(log.retired(), vec![peer]);

    assert!(transport.disconnect_attempt(attempt).await?.is_none());
    assert_eq!(
        log.retired(),
        vec![peer],
        "a retired attempt cannot retire again"
    );
    Ok(())
}

/// Facts are per generation: a read generation is reported retired, and a replacement admitted
/// and retired before the application reads it leaves no fact of its own.
#[tokio::test]
async fn test_each_generation_is_announced_in_its_own_transitions() -> Result<()> {
    let (transport, log) = transport_with_log()?;
    let peer = SecretKey::random().address().into();
    let old = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(old)?);
    assert_eq!(log.events(), vec![SwarmEvent::PeerAdmitted { peer }]);
    assert!(transport.disconnect_attempt(old).await?.is_some());

    let replacement = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(replacement)?);
    assert!(transport.disconnect_attempt(replacement).await?.is_some());
    assert_eq!(log.events(), vec![
        SwarmEvent::PeerAdmitted { peer },
        SwarmEvent::PeerRetired { peer },
    ]);
    Ok(())
}

/// A disconnect reports whether the topology referenced the peer in the state the removal was
/// applied to: an admitted peer no slot holds is removed as `Unreferenced`, an admitted
/// successor as `Referenced`.
#[tokio::test]
async fn test_disconnect_reports_whether_the_topology_referenced_the_peer() -> Result<()> {
    let (transport, _log) = transport_with_log()?;
    let unreferenced = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(unreferenced).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    assert_eq!(
        transport.disconnect_attempt(attempt).await?,
        Some(TopologyRemoval::Unreferenced)
    );

    let referenced = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(referenced).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    transport.dht.admit_connected(referenced, None)?;
    assert_eq!(
        transport.disconnect_attempt(attempt).await?,
        Some(TopologyRemoval::Referenced)
    );
    Ok(())
}

/// A topology prune that keeps the connection record (a `Disconnected` transport allowed to
/// recover) reports nothing; the retirement is reported once the record itself is retired.
#[tokio::test]
async fn test_topology_prune_keeps_the_record_and_reports_nothing() -> Result<()> {
    let (transport, log) = transport_with_log()?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    assert_eq!(log.events(), vec![SwarmEvent::PeerAdmitted { peer }]);
    transport.dht.admit_connected(peer, None)?;

    assert!(transport
        .remove_unavailable_topology(peer, Some(attempt))?
        .is_some());
    assert!(transport.is_active_connection_attempt(attempt));
    assert!(log.retired().is_empty());

    assert!(transport.disconnect_attempt(attempt).await?.is_some());
    assert_eq!(log.retired(), vec![peer]);
    Ok(())
}

/// Capacity eviction of an admitted peer the topology no longer references retires the record
/// through the same transition and is reported like any other retirement; a referenced peer is
/// kept and reports nothing.
#[tokio::test]
async fn test_eviction_of_an_unreferenced_peer_is_reported_retired() -> Result<()> {
    let (transport, log) = transport_with_log()?;
    let referenced = SecretKey::random().address().into();
    let unreferenced = SecretKey::random().address().into();
    let kept = transport.reserve_pending_connection(referenced).await?;
    assert!(transport.activate_connection_for_test(kept)?);
    transport.dht.admit_connected(referenced, None)?;
    let evicted = transport.reserve_pending_connection(unreferenced).await?;
    assert!(transport.activate_connection_for_test(evicted)?);
    assert_eq!(log.events(), vec![
        SwarmEvent::PeerAdmitted { peer: referenced },
        SwarmEvent::PeerAdmitted { peer: unreferenced },
    ]);

    assert_eq!(
        transport.retire_unless_referenced(kept).await?,
        RetirementOutcome::Declined
    );
    assert!(log.retired().is_empty());
    assert_eq!(
        transport.retire_unless_referenced(evicted).await?,
        RetirementOutcome::Retired(())
    );
    assert_eq!(log.retired(), vec![unreferenced]);
    assert!(!transport.is_active_connection_attempt(evicted));
    Ok(())
}

/// Through the production admission and terminal paths, the application reads
/// `PeerAdmitted`, then `PeerRetired`: the physical terminal state itself is never announced.
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_retirement_is_announced_after_the_read_admission() -> Result<()> {
    let transport = default_transport()?;
    let log = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    let peer = pending.peer;
    pending.admit(&transport).await?;
    let attempt = transport
        .active_attempt(peer)?
        .ok_or(Error::SwarmMissTransport(peer))?;
    assert_eq!(log.events(), vec![SwarmEvent::PeerAdmitted { peer }]);

    pending
        .callback
        .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Failed)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert!(!transport.is_active_connection_attempt(attempt));
    assert_eq!(log.events(), vec![
        SwarmEvent::PeerAdmitted { peer },
        SwarmEvent::PeerRetired { peer },
    ]);
    Ok(())
}
