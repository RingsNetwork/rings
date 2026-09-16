//! The retirement law: for every connection generation, `Connected` started ⟺ `PeerRetired`
//! started, and a topology prune that keeps the record delivers nothing.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;

use super::pending::RetirementOutcome;
use super::*;
use crate::dht::Chord;
use crate::swarm::callback::SwarmEvent;

/// Records every swarm event the application was told about, in start order.
#[derive(Default)]
struct EventLog {
    events: Mutex<Vec<SwarmEvent>>,
}

impl EventLog {
    /// Every event so far, in start order.
    fn events(&self) -> Vec<SwarmEvent> {
        self.events
            .lock()
            .expect("event log is never poisoned")
            .clone()
    }

    /// Peers reported retired so far, in start order.
    fn retired(&self) -> Vec<Did> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                SwarmEvent::PeerRetired { peer } => Some(peer),
                SwarmEvent::ConnectionStateChange { .. } => None,
            })
            .collect()
    }
}

#[async_trait]
impl SwarmCallback for EventLog {
    /// Record the event.
    async fn on_event(
        &self,
        event: &SwarmEvent,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        self.events
            .lock()
            .expect("event log is never poisoned")
            .push(event.clone());
        Ok(())
    }
}

/// A transport whose application callback is a fresh event log.
fn transport_with_log() -> Result<(SwarmTransport, Arc<EventLog>)> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let log = Arc::new(EventLog::default());
    transport.callback_slot().replace(log.clone())?;
    Ok((transport, log))
}

/// An admission that was announced is reported retired exactly once, whichever retirement
/// path ends it.
#[tokio::test]
async fn test_announced_admission_is_reported_retired_once() -> Result<()> {
    let (transport, log) = transport_with_log()?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    assert!(transport.mark_admission_announced(attempt)?);

    assert!(transport.disconnect_attempt(attempt).await?);
    assert_eq!(log.retired(), vec![peer]);

    assert!(!transport.disconnect_attempt(attempt).await?);
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

    assert!(transport.disconnect_attempt(attempt).await?);
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
    assert!(transport.disconnect_attempt(old).await?);
    assert!(!transport.mark_admission_announced(old)?);

    let replacement = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(replacement)?);
    assert!(transport.disconnect_attempt(replacement).await?);
    assert_eq!(
        log.retired(),
        vec![peer],
        "the unannounced replacement generation retires silently"
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
    transport.dht.join(peer)?;

    assert!(transport
        .remove_unavailable_topology(peer, Some(attempt))?
        .is_some());
    assert!(transport.is_active_connection_attempt(attempt));
    assert!(log.retired().is_empty());

    assert!(transport.disconnect_attempt(attempt).await?);
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
    transport.dht.join(referenced)?;
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
    let state = |state| SwarmEvent::ConnectionStateChange { peer, state };
    assert_eq!(log.events(), vec![
        state(WebrtcConnectionState::Connecting),
        state(WebrtcConnectionState::Connected),
        SwarmEvent::PeerRetired { peer },
        state(WebrtcConnectionState::Failed),
    ]);
    Ok(())
}
