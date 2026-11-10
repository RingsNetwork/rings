use super::*;

fn valid_test_range() -> WebrtcUdpPortRange {
    match WebrtcUdpPortRange::new(49160, 49200) {
        Ok(range) => range,
        Err(error) => panic!("valid range rejected: {error}"),
    }
}

#[test]
fn test_native_udp_range_builds_ephemeral_udp_with_same_bounds() {
    let udp = ephemeral_udp_for_range(valid_test_range());
    let udp = match udp {
        Ok(udp) => udp,
        Err(error) => panic!("valid range rejected by ICE stack: {error}"),
    };

    assert_eq!(udp.port_min(), 49160);
    assert_eq!(udp.port_max(), 49200);
}

#[test]
fn test_native_transport_keeps_configured_udp_range() {
    let range = valid_test_range();
    let transport = WebrtcTransport::new("", None, Some(range));

    assert_eq!(transport.udp_port_range, Some(range));
}

#[test]
fn test_external_address_candidates_split_trim_and_deduplicate() {
    let candidates = external_address_candidates(Some(" 127.0.0.1, 192.168.215.2,127.0.0.1, "));

    assert_eq!(candidates, vec![
        "127.0.0.1".to_string(),
        "192.168.215.2".to_string()
    ]);
}

#[test]
fn test_external_address_candidates_ignore_blank_config() {
    assert!(external_address_candidates(Some("  ,  ")).is_empty());
    assert!(external_address_candidates(None).is_empty());
}

#[test]
fn test_loopback_external_addresses_are_sdp_only_candidates() {
    let candidates = vec!["127.0.0.1".to_string(), "192.168.215.2".to_string()];

    assert_eq!(nat_1to1_host_candidates(&candidates), vec!["192.168.215.2"]);
    assert_eq!(sdp_extra_host_candidates(&candidates), vec!["127.0.0.1"]);
}

#[test]
fn test_append_sdp_extra_host_candidates_duplicates_host_candidates() {
    let sdp = "v=0\r\n\
a=candidate:1 1 udp 2130706431 192.168.215.2 49160 typ host\r\n\
a=end-of-candidates\r\n"
        .to_string();

    let rewritten = append_sdp_extra_host_candidates(sdp, &["127.0.0.1".to_string()]);

    assert!(rewritten.contains(
        "a=candidate:1 1 udp 2130706431 192.168.215.2 49160 typ host\r\n\
a=candidate:1 1 udp 2130706431 127.0.0.1 49160 typ host\r\n"
    ));
    assert!(rewritten.ends_with("a=end-of-candidates\r\n"));
}

#[tokio::test]
async fn test_real_peer_connection_concurrent_close_waits_for_first_cleanup_before_witness(
) -> Result<()> {
    let transport = WebrtcTransport::new("", None, None);
    let (webrtc_conn, _) = create_peer_connection(&transport).await?;
    let webrtc_conn = Arc::new(webrtc_conn);
    let entered_closed_callback = Arc::new(tokio::sync::Notify::new());
    let release_closed_callback = Arc::new(tokio::sync::Notify::new());

    {
        let entered = Arc::clone(&entered_closed_callback);
        let release = Arc::clone(&release_closed_callback);
        webrtc_conn.on_peer_connection_state_change(Box::new(
            move |state: RTCPeerConnectionState| {
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                Box::pin(async move {
                    if state == RTCPeerConnectionState::Closed {
                        entered.notify_one();
                        release.notified().await;
                    }
                })
            },
        ));
    }

    let physical_close_completed = PhysicalCloseCompletion::new();
    let witness = physical_close_completed.witness();

    let first_close = tokio::spawn({
        let webrtc_conn = Arc::clone(&webrtc_conn);
        let physical_close_completed = physical_close_completed.clone();
        async move { close_native_connection(webrtc_conn, physical_close_completed).await }
    });

    tokio::time::timeout(Duration::from_secs(5), entered_closed_callback.notified())
        .await
        .expect("first RTCPeerConnection::close must enter Closed state callback");
    assert!(!witness.is_complete());

    let second_completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let second_close = tokio::spawn({
        let webrtc_conn = Arc::clone(&webrtc_conn);
        let physical_close_completed = physical_close_completed.clone();
        let second_completed = Arc::clone(&second_completed);
        async move {
            let result = close_failed_native_send(webrtc_conn, physical_close_completed).await;
            second_completed.store(true, Ordering::Release);
            result
        }
    });

    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(
        !second_completed.load(Ordering::Acquire),
        "second close on the same connection must not return while the first close is still running"
    );
    assert!(
        !witness.is_complete(),
        "NativePhysicalCloseWitness must remain incomplete while original cleanup is paused"
    );

    release_closed_callback.notify_one();
    tokio::time::timeout(Duration::from_secs(5), first_close)
        .await
        .expect("first close timed out")
        .expect("first close task join")?;
    tokio::time::timeout(Duration::from_secs(5), second_close)
        .await
        .expect("second close timed out")
        .expect("second close task join")?;
    assert!(witness.is_complete());
    assert!(witness.completed().await);
    Ok(())
}

struct OrderedStateCallback {
    block_first_connecting: Option<Arc<tokio::sync::Notify>>,
    entered_first_connecting: Option<Arc<tokio::sync::Notify>>,
    active_callbacks: AtomicUsize,
    max_concurrent: AtomicUsize,
    observed: std::sync::Mutex<Vec<WebrtcConnectionState>>,
    on_failed_close: std::sync::Mutex<Option<ConnectionRef<WebrtcConnection>>>,
    closed_notified: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl crate::core::callback::TransportCallback for OrderedStateCallback {
    async fn on_peer_connection_state_change(
        &self,
        _cid: &str,
        state: WebrtcConnectionState,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let concurrent = self.active_callbacks.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_concurrent.fetch_max(concurrent, Ordering::AcqRel);

        let is_first = {
            let mut observed = lock_recover(&self.observed);
            let is_first = observed.is_empty();
            observed.push(state);
            is_first
        };

        if is_first && state == WebrtcConnectionState::Connecting {
            if let Some(entered) = &self.entered_first_connecting {
                entered.notify_one();
            }
            if let Some(block) = &self.block_first_connecting {
                block.notified().await;
            }
        }

        if state == WebrtcConnectionState::Failed {
            let conn = lock_recover(&self.on_failed_close).take();
            if let Some(conn) = conn {
                let witness = conn
                    .physical_close_witness()
                    .expect("live connection exposes physical close witness");
                conn.close().await.map_err(Box::new)?;
                assert!(
                    witness.is_complete(),
                    "close() inside Failed callback must await physical close completion"
                );
            }
        }

        if state == WebrtcConnectionState::Closed {
            self.closed_notified.notify_waiters();
        }

        self.active_callbacks.fetch_sub(1, Ordering::AcqRel);
        Ok(())
    }
}

#[tokio::test]
async fn test_peer_connection_state_dispatch_returns_immediately_and_drains_in_fifo_order() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let closed_notified = Arc::new(tokio::sync::Notify::new());
    let observed_callback = Arc::new(OrderedStateCallback {
        block_first_connecting: Some(Arc::clone(&release)),
        entered_first_connecting: Some(Arc::clone(&entered)),
        active_callbacks: AtomicUsize::new(0),
        max_concurrent: AtomicUsize::new(0),
        observed: std::sync::Mutex::new(Vec::new()),
        on_failed_close: std::sync::Mutex::new(None),
        closed_notified: Arc::clone(&closed_notified),
    });

    struct ForwardCallback(Arc<OrderedStateCallback>);
    #[async_trait]
    impl crate::core::callback::TransportCallback for ForwardCallback {
        async fn on_peer_connection_state_change(
            &self,
            cid: &str,
            s: WebrtcConnectionState,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            self.0.on_peer_connection_state_change(cid, s).await
        }
    }

    let inner_cb = Arc::new(InnerTransportCallback::new(
        "peer",
        Box::new(ForwardCallback(Arc::clone(&observed_callback))),
        Notifier::default(),
        NodeFramePool::new(),
    ));
    let connection_state = ConnectionStateCell::new();
    let queue = Arc::new(std::sync::Mutex::new(PeerStateQueue::default()));

    dispatch_peer_connection_state(
        &queue,
        &inner_cb,
        &connection_state,
        RTCPeerConnectionState::Connecting,
    );
    assert_eq!(
        connection_state.snapshot().webrtc(),
        WebrtcConnectionState::Connecting
    );
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("first state callback must start on runtime");

    dispatch_peer_connection_state(
        &queue,
        &inner_cb,
        &connection_state,
        RTCPeerConnectionState::Connected,
    );
    assert_eq!(
        connection_state.snapshot().webrtc(),
        WebrtcConnectionState::Connected
    );
    dispatch_peer_connection_state(
        &queue,
        &inner_cb,
        &connection_state,
        RTCPeerConnectionState::Failed,
    );
    assert_eq!(
        connection_state.snapshot().webrtc(),
        WebrtcConnectionState::Failed
    );
    dispatch_peer_connection_state(
        &queue,
        &inner_cb,
        &connection_state,
        RTCPeerConnectionState::Closed,
    );
    assert_eq!(
        connection_state.snapshot().webrtc(),
        WebrtcConnectionState::Closed
    );

    assert_eq!(
        *lock_recover(&observed_callback.observed),
        vec![WebrtcConnectionState::Connecting],
        "queued states must wait for the in-flight state callback to finish"
    );

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), closed_notified.notified())
        .await
        .expect("drainer must process all queued states through Closed");

    assert_eq!(*lock_recover(&observed_callback.observed), vec![
        WebrtcConnectionState::Connecting,
        WebrtcConnectionState::Connected,
        WebrtcConnectionState::Failed,
        WebrtcConnectionState::Closed,
    ]);
    assert_eq!(observed_callback.max_concurrent.load(Ordering::Acquire), 1);
}

fn corrupt_sdp_fingerprint(sdp: &str) -> String {
    let mut replaced = false;
    let mut lines = Vec::new();
    for line in sdp.lines() {
        if !replaced && line.starts_with("a=fingerprint:") {
            if let Some((prefix, digest)) = line.split_once(' ') {
                replaced = true;
                let flipped_digest = digest
                    .chars()
                    .map(|ch| match ch {
                        '0' => '1',
                        '1' => '0',
                        'A' => 'B',
                        'B' => 'A',
                        'a' => 'b',
                        'b' => 'a',
                        other => other,
                    })
                    .collect::<String>();
                lines.push(format!("{prefix} {flipped_digest}"));
            } else {
                lines.push(line.to_string());
            }
        } else {
            lines.push(line.to_string());
        }
    }
    assert!(replaced, "SDP must contain an a=fingerprint line");
    let mut out = lines.join("\r\n");
    out.push_str("\r\n");
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_failed_callback_closes_connection_without_deadlocking_and_preserves_state_order(
) -> Result<()> {
    let closed_notified = Arc::new(tokio::sync::Notify::new());
    let callback_state = Arc::new(OrderedStateCallback {
        block_first_connecting: None,
        entered_first_connecting: None,
        active_callbacks: AtomicUsize::new(0),
        max_concurrent: AtomicUsize::new(0),
        observed: std::sync::Mutex::new(Vec::new()),
        on_failed_close: std::sync::Mutex::new(None),
        closed_notified: Arc::clone(&closed_notified),
    });

    struct ForwardCallback(Arc<OrderedStateCallback>);
    #[async_trait]
    impl crate::core::callback::TransportCallback for ForwardCallback {
        async fn on_peer_connection_state_change(
            &self,
            cid: &str,
            s: WebrtcConnectionState,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            self.0.on_peer_connection_state_change(cid, s).await
        }
    }

    let offerer = WebrtcTransport::new("", None, None);
    let answerer = WebrtcTransport::new("", None, None);
    let sender = offerer
        .new_connection(
            "answerer",
            Box::new(ForwardCallback(Arc::clone(&callback_state))),
        )
        .await?;
    let receiver = answerer
        .new_connection(
            "offerer",
            Box::new(ForwardCallback(Arc::new(OrderedStateCallback {
                block_first_connecting: None,
                entered_first_connecting: None,
                active_callbacks: AtomicUsize::new(0),
                max_concurrent: AtomicUsize::new(0),
                observed: std::sync::Mutex::new(Vec::new()),
                on_failed_close: std::sync::Mutex::new(None),
                closed_notified: Arc::new(tokio::sync::Notify::new()),
            }))),
        )
        .await?;

    let witness = sender.physical_close_witness()?;
    *lock_recover(&callback_state.on_failed_close) = Some(sender.clone());

    let offer = sender.webrtc_create_offer().await?;
    let answer = receiver.webrtc_answer_offer(offer).await?;
    let tampered_answer = corrupt_sdp_fingerprint(&answer);
    sender.webrtc_accept_answer(tampered_answer).await?;

    tokio::time::timeout(Duration::from_secs(10), closed_notified.notified())
        .await
        .expect("Failed callback must close connection and deliver Closed without deadlocking");

    assert!(witness.is_complete());
    assert!(witness.completed().await);
    let observed = lock_recover(&callback_state.observed).clone();
    let failed_pos = observed
        .iter()
        .position(|state| *state == WebrtcConnectionState::Failed)
        .expect("Failed state must be observed");
    let closed_pos = observed
        .iter()
        .position(|state| *state == WebrtcConnectionState::Closed)
        .expect("Closed state must be observed");
    assert!(
        failed_pos < closed_pos,
        "Failed callback must complete before Closed callback runs: {observed:?}"
    );
    assert_eq!(callback_state.max_concurrent.load(Ordering::Acquire), 1);

    let _ = receiver.close().await;
    Ok(())
}
