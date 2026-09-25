//! The retirement law: for every retired connection generation, `Connected` started ⟺
//! `PeerRetired` started, and a topology prune that keeps the record delivers nothing.

use std::sync::Arc;
#[cfg(feature = "dummy")]
use std::sync::Mutex;
#[cfg(feature = "dummy")]
use std::sync::OnceLock;

use super::pending::RetirementOutcome;
use super::*;
use crate::dht::topology::TopologyRemoval;
use crate::swarm::callback::PeerLink;
#[cfg(feature = "dummy")]
use crate::swarm::callback::PeerTransition;

/// A transport whose application callback is a fresh event log.
fn transport_with_log() -> Result<(SwarmTransport, Arc<EventLog>)> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let log = Arc::new(EventLog::default());
    transport.callback_slot().replace(log.clone())?;
    Ok((transport, log))
}

/// An admission that was announced is reported retired exactly once; a second retirement of
/// the same attempt is not one.
#[tokio::test]
async fn test_announced_admission_is_reported_retired_once() -> Result<()> {
    let (transport, log) = transport_with_log()?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    assert!(transport.mark_admission_announced(attempt)?);

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

/// An admission that was never announced retires silently, so the application never sees a
/// departure without an admission.
#[tokio::test]
async fn test_unannounced_admission_retires_silently() -> Result<()> {
    let (transport, log) = transport_with_log()?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);

    assert!(transport.disconnect_attempt(attempt).await?.is_some());
    assert!(log.retired().is_empty());
    Ok(())
}

/// The announcement mark is per generation: once the record is retired, the same attempt can
/// no longer be announced, and a later generation starts unannounced.
#[tokio::test]
async fn test_announcement_is_bound_to_the_active_generation() -> Result<()> {
    let (transport, log) = transport_with_log()?;
    let peer = SecretKey::random().address().into();
    let old = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(old)?);
    assert!(transport.mark_admission_announced(old)?);
    assert!(transport.disconnect_attempt(old).await?.is_some());
    assert!(!transport.mark_admission_announced(old)?);

    let replacement = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(replacement)?);
    assert!(transport.disconnect_attempt(replacement).await?.is_some());
    assert_eq!(
        log.retired(),
        vec![peer],
        "the unannounced replacement generation retires silently"
    );
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
    assert!(transport.mark_admission_announced(attempt)?);
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
    assert!(transport.mark_admission_announced(kept)?);
    transport.dht.admit_connected(referenced, None)?;
    let evicted = transport.reserve_pending_connection(unreferenced).await?;
    assert!(transport.activate_connection_for_test(evicted)?);
    assert!(transport.mark_admission_announced(evicted)?);

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

/// Through the production admission and terminal paths, the application observes
/// `Connected`, then `PeerRetired`, then the terminal state: the retirement is decided and
/// announced before the physical terminal event is reported. The dummy transport reports
/// `Connecting` while its data channel opens, before the admission.
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_retirement_is_reported_between_admission_and_terminal_state() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let log = Arc::new(EventLog::default());
    transport.callback_slot().replace(log.clone())?;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), log.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), log.clone())
        .with_pending_connection_attempt(attempt);
    callback
        .on_data_channel_open(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert!(transport.is_active_connection_attempt(attempt));

    callback
        .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Failed)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert!(!transport.is_active_connection_attempt(attempt));
    let generation = Some(attempt.generation());
    let state = |state| SwarmEvent::ConnectionStateChange {
        peer,
        state,
        generation,
    };
    assert_eq!(log.events(), vec![
        state(WebrtcConnectionState::Connecting),
        state(WebrtcConnectionState::Connected),
        SwarmEvent::PeerRetired {
            peer,
            generation: attempt.generation(),
        },
        state(WebrtcConnectionState::Failed),
    ]);
    Ok(())
}

/// Records, at the start of every event, the event and the admitted snapshot
/// (`announced_attempts`) of the transport it observes.
#[cfg(feature = "dummy")]
#[derive(Default)]
struct SnapshotLog {
    transport: OnceLock<Arc<SwarmTransport>>,
    observed: Mutex<Vec<(SwarmEvent, Vec<PendingConnectionAttempt>)>>,
}

#[cfg(feature = "dummy")]
#[async_trait]
impl SwarmCallback for SnapshotLog {
    /// Take the snapshot before the first suspension point, as a callback that publishes
    /// state must.
    async fn on_event(
        &self,
        event: &SwarmEvent,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        let snapshot = match self.transport.get() {
            Some(transport) => transport.announced_attempts()?,
            None => Vec::new(),
        };
        self.observed
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)?
            .push((event.clone(), snapshot));
        Ok(())
    }
}

/// Linearisation (#843): when `Connected` of a generation starts, the admitted snapshot already
/// holds it, and when its `PeerRetired` starts, the snapshot no longer does. Both events name
/// the generation, the same one, so an application keyed by link pairs them exactly.
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_admitted_snapshot_is_linearised_with_admission_and_retirement() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let log = Arc::new(SnapshotLog::default());
    let _ = log.transport.set(Arc::clone(&transport));
    transport.callback_slot().replace(log.clone())?;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), log.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), log.clone())
        .with_pending_connection_attempt(attempt);
    callback
        .on_data_channel_open(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(transport.announced_attempts()?, vec![attempt]);

    callback
        .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Failed)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert!(transport.announced_attempts()?.is_empty());

    let observed = log
        .observed
        .lock()
        .map_err(|_| Error::SwarmConnectionLifecycleLock)?
        .clone();
    let link = PeerLink::new(peer, attempt.generation());
    let transitions = observed
        .iter()
        .filter_map(|(event, snapshot)| {
            event
                .peer_transition()
                .map(|(observed, transition)| (observed, transition, snapshot.clone()))
        })
        .collect::<Vec<_>>();
    assert_eq!(transitions, vec![
        (link, PeerTransition::Admitted, vec![attempt]),
        (link, PeerTransition::Retired, Vec::new()),
    ]);
    Ok(())
}

/// `disconnect_link` retires only the generation it names: a stale generation of the same peer
/// changes nothing, the named one is retired and reported once.
#[tokio::test]
async fn test_disconnect_link_retires_only_its_own_generation() -> Result<()> {
    let (transport, log) = transport_with_log()?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    assert!(transport.mark_admission_announced(attempt)?);
    let stale = PeerLink::new(peer, attempt.generation().wrapping_add(1));

    assert!(!transport.disconnect_link(stale).await?);
    assert!(transport.is_active_connection_attempt(attempt));
    assert!(log.retired().is_empty());

    let link = PeerLink::new(peer, attempt.generation());
    assert!(transport.disconnect_link(link).await?);
    assert_eq!(log.retired(), vec![peer]);
    assert!(!transport.disconnect_link(link).await?);
    Ok(())
}

/// `𝓡` is the registry's total bound, `2 × (finger slots + successors + 1)`, and the snapshot
/// of a fresh transport is empty.
#[tokio::test]
async fn test_registry_capacity_is_the_lifecycle_total() -> Result<()> {
    let (transport, _log) = transport_with_log()?;
    assert_eq!(
        transport.connection_registry_capacity()?,
        2 * (crate::dht::DEFAULT_FINGER_TABLE_SIZE + transport.dht.successors().capacity() + 1)
    );
    assert!(transport.announced_attempts()?.is_empty());
    Ok(())
}
