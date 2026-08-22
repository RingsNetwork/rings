#[cfg(feature = "dummy")]
use rings_transport::core::transport::TransportInterface;

use super::*;
#[cfg(feature = "dummy")]
use crate::dht::Chord;

#[cfg(feature = "dummy")]
#[derive(Default)]
struct BlockingDisconnectMeasure {
    inner: RecordingMeasure,
    disconnect_started: AtomicBool,
    disconnect_started_notify: Notify,
    release_disconnect: Notify,
}

#[cfg(feature = "dummy")]
impl BlockingDisconnectMeasure {
    async fn wait_for_disconnect_started(&self) {
        while !self.disconnect_started.load(Ordering::SeqCst) {
            self.disconnect_started_notify.notified().await;
        }
    }

    fn release_disconnect(&self) {
        self.release_disconnect.notify_waiters();
    }
}

#[cfg(feature = "dummy")]
#[async_trait]
impl Measure for BlockingDisconnectMeasure {
    async fn incr(&self, did: Did, counter: MeasureCounter) {
        if counter == MeasureCounter::Disconnected {
            self.disconnect_started.store(true, Ordering::SeqCst);
            self.disconnect_started_notify.notify_waiters();
            self.release_disconnect.notified().await;
        }
        self.inner.incr(did, counter).await;
    }

    async fn get_count(&self, did: Did, counter: MeasureCounter) -> u64 {
        self.inner.get_count(did, counter).await
    }
}

#[cfg(feature = "dummy")]
#[async_trait]
impl BehaviourJudgement for BlockingDisconnectMeasure {
    async fn quality(&self, did: Did) -> PeerQuality {
        self.inner.quality(did).await
    }

    async fn good(&self, did: Did) -> bool {
        self.inner.good(did).await
    }
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn retirement_serializes_with_liveness_generation_updates() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);

    let (observation_tx, observation_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let marker_transport = Arc::clone(&transport);
    let marker_thread = std::thread::spawn(move || {
        marker_transport.mark_peer_liveness_connected_with_observer_for_test(attempt, || {
            let _ = observation_tx.send(());
            let _ = release_rx.recv();
        })
    });
    observation_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .map_err(|error| {
            Error::InvalidMessage(format!("liveness observation did not start: {error}"))
        })?;

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
    assert!(retirement_done_rx
        .recv_timeout(std::time::Duration::from_millis(25))
        .is_err());

    release_tx
        .send(())
        .map_err(|error| Error::InvalidMessage(format!("liveness release failed: {error}")))?;
    marker_thread
        .join()
        .map_err(|_| Error::InvalidMessage("liveness marker thread panicked".to_string()))??;
    assert_eq!(
        retirement_thread
            .join()
            .map_err(|_| Error::InvalidMessage("retirement thread panicked".to_string()))??,
        Some(())
    );

    assert!(!transport.is_admitted_connection(peer));
    assert_eq!(transport.peer_liveness_count_for_test()?, 0);
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn retirement_clears_disconnect_epoch_for_departed_peer() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    transport.force_peer_disconnected_since_ms(peer, 1)?;
    assert!(transport
        .measured_disconnects
        .lock()
        .map_err(|_| Error::SwarmConnectionLifecycleLock)?
        .contains_key(&peer));

    assert_eq!(
        transport.retire_active_connection_with(attempt, |_| Ok(()))?,
        Some(())
    );
    assert!(!transport
        .measured_disconnects
        .lock()
        .map_err(|_| Error::SwarmConnectionLifecycleLock)?
        .contains_key(&peer));
    Ok(())
}

#[tokio::test]
async fn retirement_shuts_down_outbound_scheduler_for_departed_peer() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);

    let _handle = transport.outbound_schedulers.handle(peer);
    assert_eq!(transport.outbound_schedulers.peer_count_for_test(), 1);

    assert_eq!(
        transport.retire_active_connection_with(attempt, |_| Ok(()))?,
        Some(())
    );

    assert_eq!(transport.outbound_schedulers.peer_count_for_test(), 0);
    Ok(())
}

#[tokio::test]
async fn failed_dht_retirement_preserves_active_peer_state() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    transport.mark_peer_liveness_connected(attempt);
    transport
        .pending_finger_updates
        .lock()
        .map_err(|_| Error::SwarmConnectionLifecycleLock)?
        .entry(attempt)
        .or_default()
        .insert(3, None);

    let result = transport.retire_active_connection_with(attempt, |_| -> Result<()> {
        Err(Error::InvalidMessage(
            "injected DHT retirement failure".to_string(),
        ))
    });

    assert!(matches!(result, Err(Error::InvalidMessage(_))));
    assert!(transport.is_admitted_connection_attempt(attempt));
    assert!(transport
        .peer_connected_for_ms(peer, crate::utils::get_epoch_ms_i64())?
        .is_some());
    assert!(transport
        .pending_finger_updates
        .lock()
        .map_err(|_| Error::SwarmConnectionLifecycleLock)?
        .contains_key(&attempt));
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn stale_active_evidence_cannot_retire_replacement_generation() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let peer = SecretKey::random().address().into();
    let old_attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(old_attempt)?);
    transport.dht.join(peer)?;

    assert_eq!(
        transport.retire_active_connection_with(old_attempt, |_| {
            transport.dht.remove(peer)?;
            Ok(())
        })?,
        Some(())
    );

    let replacement = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(replacement)?);
    transport.dht.join(peer)?;

    assert!(transport
        .disconnect_unavailable(old_attempt)
        .await?
        .is_none());
    assert!(transport.is_admitted_connection_attempt(replacement));
    assert!(transport.dht.successors().contains(&peer)?);
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn stale_inbound_observation_cannot_refresh_replacement_liveness() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let peer = SecretKey::random().address().into();
    let old_attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(old_attempt)?);
    let now_ms = crate::utils::get_epoch_ms_i64();
    let expired_at_ms = now_ms - PEER_LIVENESS_TIMEOUT_MS - 1;
    transport.force_peer_liveness_probe_sent_at(peer, expired_at_ms)?;
    assert!(transport
        .peer_liveness_expiry(old_attempt, now_ms)?
        .is_some());

    assert_eq!(
        transport.retire_active_connection_with(old_attempt, |_| Ok(()))?,
        Some(())
    );
    let replacement = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(replacement)?);
    transport.force_peer_liveness_probe_sent_at(peer, expired_at_ms)?;

    transport.mark_peer_liveness_inbound(old_attempt);

    assert!(transport
        .peer_liveness_expiry(replacement, now_ms)?
        .is_some());
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn stale_pending_close_cannot_remove_replacement_transport() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let old_attempt = transport.reserve_pending_connection(peer).await?;
    let old_callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback))
        .with_pending_connection_attempt(old_attempt);
    let old_connection = transport
        .new_pending_connection(old_attempt, old_callback)
        .await?
        .into_connection();
    transport.force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Failed)?;
    assert!(transport.retire_pending_connection(old_attempt)?);

    let replacement = transport.reserve_pending_connection(peer).await?;
    let replacement_callback =
        InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback))
            .with_pending_connection_attempt(replacement);
    transport
        .new_pending_connection(replacement, replacement_callback)
        .await?;

    assert!(
        !transport
            .transport
            .close_connection_if_current(&old_connection.connection)
            .await?
    );
    assert!(transport.is_pending_connection_attempt(replacement)?);
    assert!(transport.get_raw_connection(peer).is_some());
    transport.cancel_pending_connection(replacement).await?;
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn stale_terminal_callback_cannot_report_replacement_closed() -> Result<()> {
    let measure = Arc::new(BlockingDisconnectMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer = SecretKey::random().address().into();
    let callback = Arc::new(CountingSwarmCallback::default());
    let old_attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(old_attempt)?);
    transport.dht.join(peer)?;

    let terminal_transport = Arc::clone(&transport);
    let terminal_callback = callback.clone();
    let terminal = tokio::spawn(async move {
        InnerSwarmCallback::new(terminal_transport, terminal_callback)
            .with_pending_connection_attempt(old_attempt)
            .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Closed)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });

    measure.wait_for_disconnect_started().await;
    assert_eq!(
        transport.retire_active_connection_with(old_attempt, |_| {
            transport.dht.remove(peer)?;
            Ok(())
        })?,
        Some(())
    );
    let replacement = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(replacement)?);
    transport.dht.join(peer)?;
    measure.release_disconnect();

    terminal
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))??;

    assert!(transport.is_admitted_connection_attempt(replacement));
    assert!(transport.dht.successors().contains(&peer)?);
    assert!(!callback.events()?.contains(&WebrtcConnectionState::Closed));
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn pending_replacement_does_not_suppress_retired_generation_closed() -> Result<()> {
    let measure = Arc::new(BlockingDisconnectMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer = SecretKey::random().address().into();
    let callback = Arc::new(CountingSwarmCallback::default());
    let old_attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(old_attempt)?);
    transport.dht.join(peer)?;

    let terminal_transport = Arc::clone(&transport);
    let terminal_callback = callback.clone();
    let terminal = tokio::spawn(async move {
        InnerSwarmCallback::new(terminal_transport, terminal_callback)
            .with_pending_connection_attempt(old_attempt)
            .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Closed)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });

    measure.wait_for_disconnect_started().await;
    assert_eq!(
        transport.retire_active_connection_with(old_attempt, |_| {
            transport.dht.remove(peer)?;
            Ok(())
        })?,
        Some(())
    );
    let replacement = transport.reserve_pending_connection(peer).await?;
    measure.release_disconnect();

    terminal
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))??;

    assert!(transport.is_pending_connection_attempt(replacement)?);
    assert!(!transport.is_admitted_connection_attempt(replacement));
    assert!(callback.events()?.contains(&WebrtcConnectionState::Closed));
    assert!(transport.cancel_pending_connection(replacement).await?);
    Ok(())
}
