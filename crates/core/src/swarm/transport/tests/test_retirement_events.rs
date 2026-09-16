//! The retirement law: for every connection generation, `Connected` delivered ⟺ `PeerRetired`
//! delivered, and a topology prune that keeps the record delivers nothing.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;

use super::connection::UnreferencedRetirement;
use super::*;
use crate::dht::Chord;
use crate::swarm::callback::SwarmEvent;

/// Records the peers whose retirement the application was told about.
#[derive(Default)]
struct RetirementLog {
    retired: Mutex<Vec<Did>>,
}

impl RetirementLog {
    /// Peers reported retired so far, in delivery order.
    fn retired(&self) -> Vec<Did> {
        self.retired
            .lock()
            .expect("retirement log is never poisoned")
            .clone()
    }
}

#[async_trait]
impl SwarmCallback for RetirementLog {
    /// Record `PeerRetired`; every other event is irrelevant to the law under test.
    async fn on_event(
        &self,
        event: &SwarmEvent,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        if let SwarmEvent::PeerRetired { peer } = event {
            self.retired
                .lock()
                .expect("retirement log is never poisoned")
                .push(*peer);
        }
        Ok(())
    }
}

/// A transport whose application callback is a fresh retirement log.
fn transport_with_log() -> Result<(SwarmTransport, Arc<RetirementLog>)> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let log = Arc::new(RetirementLog::default());
    transport.callback_slot().replace(log.clone())?;
    Ok((transport, log))
}

/// An admission that was announced is reported retired exactly once, whichever retirement
/// path ends it.
#[tokio::test]
async fn announced_admission_is_reported_retired_once() -> Result<()> {
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
async fn unannounced_admission_retires_silently() -> Result<()> {
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
async fn announcement_is_bound_to_the_active_generation() -> Result<()> {
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
async fn topology_prune_keeps_the_record_and_reports_nothing() -> Result<()> {
    let (transport, log) = transport_with_log()?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    assert!(transport.mark_admission_announced(attempt)?);
    transport.dht.join(peer)?;

    assert!(transport
        .remove_unavailable_topology(peer, Some(attempt))?
        .is_some());
    assert!(transport.is_admitted_connection_attempt(attempt));
    assert!(log.retired().is_empty());

    assert!(transport.disconnect_attempt(attempt).await?);
    assert_eq!(log.retired(), vec![peer]);
    Ok(())
}

/// Capacity eviction of an admitted peer the topology no longer references retires the record
/// through the same transition and is reported like any other retirement; a referenced peer is
/// kept and reports nothing.
#[tokio::test]
async fn eviction_of_an_unreferenced_peer_is_reported_retired() -> Result<()> {
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

    assert!(matches!(
        transport.retire_unless_referenced(kept).await?,
        UnreferencedRetirement::Referenced
    ));
    assert!(log.retired().is_empty());
    assert!(matches!(
        transport.retire_unless_referenced(evicted).await?,
        UnreferencedRetirement::Retired
    ));
    assert_eq!(log.retired(), vec![unreferenced]);
    assert!(!transport.is_admitted_connection_attempt(evicted));
    Ok(())
}
