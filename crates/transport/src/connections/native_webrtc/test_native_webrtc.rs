use super::*;

fn valid_test_range() -> UdpPortRange {
    match UdpPortRange::new(49160, 49200) {
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

#[test]
fn test_excluded_virtual_interfaces_and_split_remote_offer_candidates() {
    assert!(is_unrouted_container_bridge_interface("docker0"));
    assert!(is_unrouted_container_bridge_interface("br-9a1b2c3d4e5f"));
    assert!(is_unrouted_container_bridge_interface("veth42a1b0"));
    assert!(is_unrouted_container_bridge_interface("virbr0"));
    assert!(is_unrouted_container_bridge_interface("cni0"));
    assert!(is_unrouted_container_bridge_interface("flannel.1"));
    assert!(is_unrouted_container_bridge_interface("cali1234"));
    assert!(!is_unrouted_container_bridge_interface("br0"));
    assert!(!is_unrouted_container_bridge_interface("br-lan"));
    assert!(!is_unrouted_container_bridge_interface("tunl0"));
    assert!(!is_unrouted_container_bridge_interface("wg0"));
    assert!(!is_unrouted_container_bridge_interface("eth0"));
    assert!(!is_unrouted_container_bridge_interface("ens5"));
    assert!(!is_unrouted_container_bridge_interface("en0"));
    assert!(!is_unrouted_container_bridge_interface("wlan0"));

    let offer_sdp = "v=0\r\n\
o=- 1 2 IN IP4 127.0.0.1\r\n\
a=ice-ufrag:ufrag1\r\n\
a=ice-pwd:password1234567890123456\r\n\
a=candidate:1 1 udp 2130706431 10.0.0.2 51345 typ host\r\n\
a=candidate:2 1 udp 1677729535 14.155.204.30 51345 typ srflx raddr 0.0.0.0 rport 0\r\n\
a=end-of-candidates\r\n";
    let (stripped, candidates) = split_sdp_remote_candidates(offer_sdp);
    assert!(!stripped.contains("a=candidate:"));
    assert!(!stripped.contains("a=end-of-candidates"));
    assert!(stripped.contains("a=ice-ufrag:ufrag1"));
    assert_eq!(candidates, vec![
        "candidate:1 1 udp 2130706431 10.0.0.2 51345 typ host".to_string(),
        "candidate:2 1 udp 1677729535 14.155.204.30 51345 typ srflx raddr 0.0.0.0 rport 0"
            .to_string(),
    ]);
}

/// Regression witness for #1290 Item 1:
/// 1. When `answer_offer` returns its local Answer SDP, the responder must not yet have
///    fired outbound UDP STUN connectivity checks at the offerer's candidate port (preventing
///    pre-answer NAT port collisions while the HTTP `answerOffer` response is still in flight).
/// 2. Once `WEBRTC_ANSWER_CHECK_START_DELAY` elapses on a live connection, the deferred remote
///    candidates are added and the responder sends its outbound STUN Binding Request.
/// 3. If the responder connection is closed before `WEBRTC_ANSWER_CHECK_START_DELAY` elapses,
///    the deferred injection task cancels cleanly and sends zero UDP packets.
#[tokio::test]
async fn test_answer_offer_defers_outbound_stun_checks_until_after_answer_returns() -> Result<()> {
    struct NoopCallback;
    impl crate::core::callback::TransportCallback for NoopCallback {}

    let offerer_transport = WebrtcTransport::new("", None, None);
    let offerer = offerer_transport
        .new_connection("responder", Box::new(NoopCallback))
        .await?;
    let real_offer_bytes = offerer.create_offer().await?;
    let real_offer_sdp = String::from_utf8(real_offer_bytes.to_vec()).expect("UTF-8 offer SDP");
    let (stripped_base_offer, _) = split_sdp_remote_candidates(&real_offer_sdp);
    let _ = offerer.close().await;

    // Case A: Live responder with a probe UDP socket acting as the offerer's NAT port.
    let probe_socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("probe UDP socket must bind");
    let probe_addr = probe_socket.local_addr().expect("probe local_addr");
    let synthetic_offer = format!(
        "{stripped_base_offer}a=candidate:1 1 udp 2130706431 127.0.0.1 {} typ host\r\n",
        probe_addr.port()
    );

    let responder_transport = WebrtcTransport::new("", Some("127.0.0.1".to_string()), None);
    let responder = responder_transport
        .new_connection("offerer_live", Box::new(NoopCallback))
        .await?;
    let answer_bytes = responder
        .answer_offer(Bytes::from(synthetic_offer.clone()))
        .await?;
    let answer_sdp = String::from_utf8(answer_bytes.to_vec()).expect("UTF-8 answer SDP");
    assert!(
        answer_sdp.contains("a=candidate:"),
        "responder answer SDP must contain gathered local candidates"
    );

    let mut buf = [0u8; 256];
    assert!(
        matches!(
            probe_socket.try_recv(&mut buf),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
        ),
        "responder must not send any UDP STUN packet to the offerer's port before answer_offer returns"
    );

    // After WEBRTC_ANSWER_CHECK_START_DELAY (450 ms), the deferred candidate is added and a STUN
    // Binding Request arrives on `probe_socket`.
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), probe_socket.recv_from(&mut buf))
        .await
        .expect("responder must send STUN check after WEBRTC_ANSWER_CHECK_START_DELAY")
        .expect("probe_socket recv_from");
    assert!(
        n >= 20 && buf[0] == 0x00 && buf[1] == 0x01,
        "received packet must be a STUN Binding Request (0x0001)"
    );
    let _ = responder.close().await;

    // Case B: Responder closed immediately after answer_offer returns must cancel the
    // deferred candidate injection and send zero packets.
    let cancelled_probe = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("cancelled probe UDP socket must bind");
    let cancelled_addr = cancelled_probe.local_addr().expect("cancelled local_addr");
    let cancelled_offer = format!(
        "{stripped_base_offer}a=candidate:1 1 udp 2130706431 127.0.0.1 {} typ host\r\n",
        cancelled_addr.port()
    );
    let responder_cancelled = responder_transport
        .new_connection("offerer_cancelled", Box::new(NoopCallback))
        .await?;
    let _ = responder_cancelled
        .answer_offer(Bytes::from(cancelled_offer))
        .await?;
    responder_cancelled.close().await?;
    tokio::time::sleep(WEBRTC_ANSWER_CHECK_START_DELAY + Duration::from_millis(150)).await;
    assert!(
        matches!(
            cancelled_probe.try_recv(&mut buf),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
        ),
        "closed responder must cancel deferred candidate checks and send zero UDP packets"
    );
    Ok(())
}

#[tokio::test]
async fn test_real_peer_connection_concurrent_close_waits_for_first_cleanup_before_witness(
) -> Result<()> {
    let transport = WebrtcTransport::new("", None, None);
    let (webrtc_conn, _, _, _) = create_peer_connection(&transport).await?;
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

    struct NoopCallback;
    impl crate::core::callback::TransportCallback for NoopCallback {}

    let second_completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let second_close = tokio::spawn({
        let webrtc_conn = Arc::clone(&webrtc_conn);
        let physical_close_completed = physical_close_completed.clone();
        let second_completed = Arc::clone(&second_completed);
        async move {
            let channel_pool = Arc::new(ChannelPool::from_vec(Vec::new()));
            let data_channel_notifier = Notifier::default();
            let inner_cb = Arc::new(InnerTransportCallback::new(
                "peer",
                Box::new(NoopCallback),
                data_channel_notifier.clone(),
                NodeFramePool::new(),
            ));
            let result = close_failed_native_send(
                Arc::new(Mutex::new(())),
                Arc::new(std::sync::RwLock::new(webrtc_conn)),
                channel_pool,
                data_channel_notifier,
                inner_cb,
                Arc::new(std::sync::Mutex::new(None)),
                physical_close_completed,
            )
            .await;
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
    observed: std::sync::Mutex<Vec<TransportConnectionState>>,
    on_failed_close: std::sync::Mutex<Option<ConnectionRef<WebrtcConnection>>>,
    closed_notified: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl crate::core::callback::TransportCallback for OrderedStateCallback {
    async fn on_connection_state_change(
        &self,
        _cid: &str,
        state: TransportConnectionState,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let concurrent = self.active_callbacks.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_concurrent.fetch_max(concurrent, Ordering::AcqRel);

        let is_first = {
            let mut observed = lock_recover(&self.observed);
            let is_first = observed.is_empty();
            observed.push(state);
            is_first
        };

        if is_first && state == TransportConnectionState::Connecting {
            if let Some(entered) = &self.entered_first_connecting {
                entered.notify_one();
            }
            if let Some(block) = &self.block_first_connecting {
                block.notified().await;
            }
        }

        if state == TransportConnectionState::Failed {
            let conn = lock_recover(&self.on_failed_close).take();
            if let Some(conn) = conn {
                let witness = conn
                    .upgrade()
                    .expect("live connection exposes physical close witness")
                    .physical_close_witness();
                conn.close().await.map_err(Box::new)?;
                assert!(
                    witness.is_complete(),
                    "close() inside Failed callback must await physical close completion"
                );
            }
        }

        if state == TransportConnectionState::Closed {
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
        async fn on_connection_state_change(
            &self,
            cid: &str,
            s: TransportConnectionState,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            self.0.on_connection_state_change(cid, s).await
        }
    }

    let inner_cb = Arc::new(InnerTransportCallback::new(
        "peer",
        Box::new(ForwardCallback(Arc::clone(&observed_callback))),
        Notifier::default(),
        NodeFramePool::new(),
    ));
    let connection_state = ConnectionStateCell::new();
    let observe_and_dispatch = |state: RTCPeerConnectionState| {
        let state_change: TransportConnectionState = state.into();
        connection_state.observe_phase(state_change);
        inner_cb.dispatch_peer_connection_state(state_change);
    };

    observe_and_dispatch(RTCPeerConnectionState::Connecting);
    assert_eq!(
        connection_state.snapshot().state(),
        TransportConnectionState::Connecting
    );
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("first state callback must start on runtime");

    observe_and_dispatch(RTCPeerConnectionState::Connected);
    assert_eq!(
        connection_state.snapshot().state(),
        TransportConnectionState::Connected
    );
    observe_and_dispatch(RTCPeerConnectionState::Failed);
    assert_eq!(
        connection_state.snapshot().state(),
        TransportConnectionState::Failed
    );
    observe_and_dispatch(RTCPeerConnectionState::Closed);
    assert_eq!(
        connection_state.snapshot().state(),
        TransportConnectionState::Closed
    );

    assert_eq!(
        *lock_recover(&observed_callback.observed),
        vec![TransportConnectionState::Connecting],
        "queued states must wait for the in-flight state callback to finish"
    );

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), closed_notified.notified())
        .await
        .expect("drainer must process all queued states through Closed");

    assert_eq!(*lock_recover(&observed_callback.observed), vec![
        TransportConnectionState::Connecting,
        TransportConnectionState::Connected,
        TransportConnectionState::Failed,
        TransportConnectionState::Closed,
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
        async fn on_connection_state_change(
            &self,
            cid: &str,
            s: TransportConnectionState,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            self.0.on_connection_state_change(cid, s).await
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

    let witness = sender.upgrade()?.physical_close_witness();
    *lock_recover(&callback_state.on_failed_close) = Some(sender.clone());

    let offer = sender.create_offer().await?;
    let answer = receiver.answer_offer(offer).await?;
    let answer_sdp = String::from_utf8(answer.to_vec()).expect("UTF-8 answer SDP");
    let tampered_answer = corrupt_sdp_fingerprint(&answer_sdp);
    sender.accept_answer(Bytes::from(tampered_answer)).await?;

    tokio::time::timeout(Duration::from_secs(10), closed_notified.notified())
        .await
        .expect("Failed callback must close connection and deliver Closed without deadlocking");

    assert!(witness.is_complete());
    assert!(witness.completed().await);
    let observed = lock_recover(&callback_state.observed).clone();
    let failed_pos = observed
        .iter()
        .position(|state| *state == TransportConnectionState::Failed)
        .expect("Failed state must be observed");
    let closed_pos = observed
        .iter()
        .position(|state| *state == TransportConnectionState::Closed)
        .expect("Closed state must be observed");
    assert!(
        failed_pos < closed_pos,
        "Failed callback must complete before Closed callback runs: {observed:?}"
    );
    assert_eq!(callback_state.max_concurrent.load(Ordering::Acquire), 1);

    let _ = receiver.close().await;
    Ok(())
}

#[tokio::test]
async fn test_turn_tcp_url_parsing_and_rfc5766_stream_framing() {
    use turn_tcp::encode_turn_tcp_frame;
    use turn_tcp::parse_turn_tcp_url;
    use turn_tcp::read_turn_tcp_frame;
    use turn_tcp::TurnTcpEndpoint;

    assert_eq!(
        parse_turn_tcp_url("turn:turn.example.com:80?transport=tcp"),
        Some(TurnTcpEndpoint {
            host: "turn.example.com".to_string(),
            port: 80,
            use_tls: false,
        })
    );
    assert_eq!(
        parse_turn_tcp_url("turns:turn.example.com:5349?transport=tcp"),
        Some(TurnTcpEndpoint {
            host: "turn.example.com".to_string(),
            port: 5349,
            use_tls: true,
        })
    );
    assert_eq!(
        parse_turn_tcp_url("turns:turn.example.com"),
        Some(TurnTcpEndpoint {
            host: "turn.example.com".to_string(),
            port: 5349,
            use_tls: true,
        })
    );
    assert_eq!(parse_turn_tcp_url("turn:turn.example.com:3478"), None);
    assert_eq!(
        parse_turn_tcp_url("turn:turn.example.com:3478?transport=udp"),
        None
    );
    assert_eq!(parse_turn_tcp_url("stun:stun.l.google.com:19302"), None);

    // Per-connection TURN pool selection (#1316, #1317): retains all STUN servers while rotating
    // across multiple TURN servers 1 at a time per connection.
    let pool = parse_ice_servers_strict(
        "stun://stun.l.google.com:19302;\
         turn://u:p@turn1.example.com:3478;\
         turn://u:p@turn2.example.com:3478;\
         turn://u:p@turn3.example.com:3478;\
         turn://u:p@turn4.example.com:3478",
    )
    .expect("valid 4-TURN pool");
    let c0 = turn_tcp::select_ice_servers_with_cursor(&pool, 0);
    let c1 = turn_tcp::select_ice_servers_with_cursor(&pool, 1);
    let c2 = turn_tcp::select_ice_servers_with_cursor(&pool, 2);
    let c3 = turn_tcp::select_ice_servers_with_cursor(&pool, 3);
    assert_eq!(c0.len(), 2);
    assert_eq!(c0[0].urls, vec!["stun:stun.l.google.com:19302"]);
    assert_eq!(c0[1].urls, vec!["turn:turn1.example.com:3478"]);
    assert_eq!(c1[1].urls, vec!["turn:turn2.example.com:3478"]);
    assert_eq!(c2[1].urls, vec!["turn:turn3.example.com:3478"]);
    assert_eq!(c3[1].urls, vec!["turn:turn4.example.com:3478"]);

    // When OS UDP is unavailable, mixed UDP+TCP pools filter to TCP TURN endpoints before
    // round-robin selection so TCP fallback is never lost.
    let mixed_pool = parse_ice_servers_strict(
        "stun://stun.l.google.com:19302;\
         turn://u:p@turn1.example.com:3478;\
         turn://u:p@turn2.example.com:80?transport=tcp",
    )
    .expect("valid mixed UDP+TCP TURN pool");
    let udp_blocked_sel = turn_tcp::select_ice_servers_with_cursor_and_udp(&mixed_pool, 0, false);
    assert_eq!(udp_blocked_sel.len(), 2);
    assert_eq!(udp_blocked_sel[1].urls, vec![
        "turn:turn2.example.com:80?transport=tcp"
    ]);
    // When OS UDP is available on a mixed UDP+TCP pool, at most 1 UDP TURN and 1 TCP TURN
    // server are selected so TCP fallback remains available if upstream firewalls block UDP.
    let udp_ok_mixed_sel = turn_tcp::select_ice_servers_with_cursor_and_udp(&mixed_pool, 0, true);
    assert_eq!(udp_ok_mixed_sel.len(), 3);
    assert_eq!(udp_ok_mixed_sel[1].urls, vec![
        "turn:turn1.example.com:3478"
    ]);
    assert_eq!(udp_ok_mixed_sel[2].urls, vec![
        "turn:turn2.example.com:80?transport=tcp"
    ]);

    // ChannelData with 5-byte payload (unaligned) must be padded to 8 bytes (total 12 bytes) over TCP.
    let unpadded_channel_data = [0x40, 0x01, 0x00, 0x05, b'h', b'e', b'l', b'l', b'o'];
    let framed_cd = encode_turn_tcp_frame(&unpadded_channel_data).expect("valid ChannelData");
    assert_eq!(framed_cd.len(), 12);
    assert_eq!(&framed_cd[..9], &unpadded_channel_data);
    assert_eq!(&framed_cd[9..], &[0, 0, 0]);

    // 20-byte STUN Binding Request with 0-byte body.
    let mut stun_pkt = [0u8; 20];
    stun_pkt[1] = 0x01;
    stun_pkt[4..8].copy_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
    let framed_stun = encode_turn_tcp_frame(&stun_pkt).expect("valid STUN header");
    assert_eq!(framed_stun, stun_pkt);

    // Coalesce both frames into one byte stream and read them back sequentially.
    let mut stream_bytes = Vec::new();
    stream_bytes.extend_from_slice(&framed_cd);
    stream_bytes.extend_from_slice(&framed_stun);
    let mut cursor = std::io::Cursor::new(stream_bytes);

    let decoded_cd = read_turn_tcp_frame(&mut cursor)
        .await
        .expect("read ChannelData frame");
    assert_eq!(decoded_cd, framed_cd);
    let decoded_stun = read_turn_tcp_frame(&mut cursor)
        .await
        .expect("read STUN frame");
    assert_eq!(decoded_stun, framed_stun);
}

struct StaticTurnAuthHandler;

impl webrtc::turn::auth::AuthHandler for StaticTurnAuthHandler {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        _src_addr: std::net::SocketAddr,
    ) -> std::result::Result<Vec<u8>, webrtc::turn::Error> {
        if username == "testuser" {
            Ok(webrtc::turn::auth::generate_auth_key(
                username, realm, "testpass",
            ))
        } else {
            Err(webrtc::turn::Error::Other("unauthorized".to_string()))
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_turn_over_tcp_establishes_webrtc_data_channel_when_os_udp_is_blocked() -> Result<()> {
    use std::net::Ipv4Addr;

    use tokio::io::AsyncWriteExt;

    // 1. Start an in-process TURN server on UDP and a TCP front-end listener that speaks RFC 5766
    //    STUN / ChannelData framing over TCP.
    let udp_turn_listener = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let udp_turn_addr = udp_turn_listener.local_addr().unwrap();
    let turn_server =
        webrtc::turn::server::Server::new(webrtc::turn::server::config::ServerConfig {
            realm: "rings.rs".to_string(),
            auth_handler: Arc::new(StaticTurnAuthHandler),
            conn_configs: vec![webrtc::turn::server::config::ConnConfig {
                conn: udp_turn_listener,
                relay_addr_generator: Box::new(
                    webrtc::turn::relay::relay_none::RelayAddressGeneratorNone {
                        address: "127.0.0.1".to_string(),
                        net: Arc::new(webrtc::util::vnet::net::Net::new(None)),
                    },
                ),
            }],
            channel_bind_timeout: Duration::from_secs(0),
            alloc_close_notify: None,
        })
        .await
        .unwrap();

    let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp_turn_addr = tcp_listener.local_addr().unwrap();
    let accepted_tcp_conns = Arc::new(AtomicUsize::new(0));
    let accepted_tcp_conns_clone = Arc::clone(&accepted_tcp_conns);
    let server_cancel = CancellationToken::new();
    let server_cancel_clone = server_cancel.clone();

    tokio::spawn(async move {
        loop {
            let accept_res = tokio::select! {
                _ = server_cancel_clone.cancelled() => break,
                res = tcp_listener.accept() => res,
            };
            let Ok((tcp_stream, _)) = accept_res else {
                break;
            };
            accepted_tcp_conns_clone.fetch_add(1, Ordering::SeqCst);
            let conn_cancel = server_cancel_clone.child_token();
            tokio::spawn(async move {
                let Ok(udp_proxy) = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await
                else {
                    return;
                };
                let udp_proxy = Arc::new(udp_proxy);
                let (mut reader, mut writer) = tokio::io::split(tcp_stream);
                let udp_for_read = Arc::clone(&udp_proxy);
                let c1 = conn_cancel.clone();
                let tcp_to_udp = async move {
                    loop {
                        let frame = tokio::select! {
                            _ = c1.cancelled() => break,
                            res = turn_tcp::read_turn_tcp_frame(&mut reader) => match res {
                                Ok(f) => f,
                                Err(_) => break,
                            },
                        };
                        if udp_for_read.send_to(&frame, udp_turn_addr).await.is_err() {
                            break;
                        }
                    }
                };
                let udp_for_write = Arc::clone(&udp_proxy);
                let c2 = conn_cancel.clone();
                let udp_to_tcp = async move {
                    let mut buf = vec![0u8; 65_535];
                    loop {
                        let (n, _) = tokio::select! {
                            _ = c2.cancelled() => break,
                            res = udp_for_write.recv_from(&mut buf) => match res {
                                Ok(v) => v,
                                Err(_) => break,
                            },
                        };
                        let Some(framed) = turn_tcp::encode_turn_tcp_frame(&buf[..n]) else {
                            continue;
                        };
                        if writer.write_all(&framed).await.is_err() || writer.flush().await.is_err()
                        {
                            break;
                        }
                    }
                };
                tokio::select! {
                    _ = tcp_to_udp => {},
                    _ = udp_to_tcp => {},
                }
            });
        }
    });

    struct MsgCaptureCallback {
        received_tx: tokio::sync::Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    }

    #[async_trait]
    impl crate::core::callback::TransportCallback for MsgCaptureCallback {
        async fn on_admitted_message(
            &self,
            message: crate::core::callback::AdmittedInboundMessage<'_>,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            if let Some(tx) = self.received_tx.lock().await.as_ref() {
                let _ = tx.send(message.payload().to_vec()).await;
            }
            Ok(())
        }
    }

    // 2. Simulate OS-level UDP EPERM for the offerer and configure both a dead UDP STUN server
    //    and the TCP TURN server (`?transport=tcp`).
    let offerer_ice = format!(
        "stun://127.0.0.1:9;turn://testuser:testpass@127.0.0.1:{}?transport=tcp",
        tcp_turn_addr.port()
    );
    let offerer = WebrtcTransport::new(&offerer_ice, None, None);

    turn_tcp::set_force_os_udp_blocked_for_test(true);
    let (offer_msg_tx, mut offer_msg_rx) = mpsc::channel(4);
    let sender_res = offerer
        .new_connection(
            "answerer",
            Box::new(MsgCaptureCallback {
                received_tx: tokio::sync::Mutex::new(Some(offer_msg_tx)),
            }),
        )
        .await;
    turn_tcp::set_force_os_udp_blocked_for_test(false);
    let sender = sender_res?;

    // 3. Answerer runs on normal OS UDP.
    let (answer_msg_tx, mut answer_msg_rx) = mpsc::channel(4);
    let answerer = WebrtcTransport::new("", Some("127.0.0.1".to_string()), None);
    let receiver = answerer
        .new_connection(
            "offerer",
            Box::new(MsgCaptureCallback {
                received_tx: tokio::sync::Mutex::new(Some(answer_msg_tx)),
            }),
        )
        .await?;

    let offer_bytes = sender.create_offer().await?;
    let offer_sdp = String::from_utf8(offer_bytes.to_vec()).expect("UTF-8 offer SDP");
    assert!(
        offer_sdp.contains("typ relay"),
        "UDP-blocked offerer with ?transport=tcp must gather a relay candidate via TCP TURN: {offer_sdp}"
    );
    assert!(
        !offer_sdp.contains("typ host"),
        "UDP-blocked offerer in VNet relay-only mode must not advertise host UDP candidates: {offer_sdp}"
    );
    assert!(
        accepted_tcp_conns.load(Ordering::SeqCst) >= 1,
        "offerer must connect to the TURN server over TCP"
    );

    let answer_sdp = receiver.answer_offer(offer_bytes).await?;
    sender.accept_answer(answer_sdp).await?;

    sender.wait_for_channel_open().await?;
    receiver.wait_for_channel_open().await?;

    let payload_a2b = Bytes::from_static(b"hello from udp-blocked node over turn tcp");
    sender.send_message(payload_a2b.clone()).await?.await?;
    let recv_at_b = tokio::time::timeout(Duration::from_secs(5), answer_msg_rx.recv())
        .await
        .expect("timed out waiting for message at answerer")
        .expect("answerer channel closed");
    assert_eq!(recv_at_b, payload_a2b.as_ref());

    let payload_b2a = Bytes::from_static(b"reply to udp-blocked node over turn tcp");
    receiver.send_message(payload_b2a.clone()).await?.await?;
    let recv_at_a = tokio::time::timeout(Duration::from_secs(5), offer_msg_rx.recv())
        .await
        .expect("timed out waiting for message at offerer")
        .expect("offerer channel closed");
    assert_eq!(recv_at_a, payload_b2a.as_ref());

    let _ = sender.close().await;
    let _ = receiver.close().await;

    // 4. Verify normal OS UDP mode (`use_vnet_relay_only == false`) with a TCP TURN server:
    //    `gather_candidates_srflx` sends a bare STUN Binding Request to the local UDP bridge,
    //    which must be rejected locally with a STUN Binding Error Response rather than forwarded
    //    over TCP (avoiding both a 5 s STUN timeout and a bogus TCP-backed `srflx` candidate).
    accepted_tcp_conns.store(0, Ordering::SeqCst);
    let normal_udp_offerer_ice = format!(
        "turn://testuser:testpass@127.0.0.1:{}?transport=tcp",
        tcp_turn_addr.port()
    );
    let normal_udp_offerer = WebrtcTransport::new(&normal_udp_offerer_ice, None, None);
    let (normal_msg_tx, _normal_msg_rx) = mpsc::channel(4);
    let normal_sender = normal_udp_offerer
        .new_connection(
            "normal_answerer",
            Box::new(MsgCaptureCallback {
                received_tx: tokio::sync::Mutex::new(Some(normal_msg_tx)),
            }),
        )
        .await?;
    let gather_started = std::time::Instant::now();
    let normal_offer_bytes = normal_sender.create_offer().await?;
    let normal_offer_sdp = String::from_utf8(normal_offer_bytes.to_vec()).expect("UTF-8 offer SDP");
    assert!(
        gather_started.elapsed() < Duration::from_secs(4),
        "normal UDP offerer with TCP TURN bridge must not stall on a 5 s STUN srflx timeout: {:?}",
        gather_started.elapsed()
    );
    assert!(
        normal_offer_sdp.contains("typ relay"),
        "normal UDP offerer with TCP TURN must gather a relay candidate: {normal_offer_sdp}"
    );
    assert!(
        !normal_offer_sdp.contains("typ srflx"),
        "TCP TURN bridge must not produce a bogus UDP srflx candidate when no STUN server is configured: {normal_offer_sdp}"
    );
    assert_eq!(
        accepted_tcp_conns.load(Ordering::SeqCst),
        1,
        "only the TURN Allocate session (not the bare STUN srflx probe) should open a TCP connection"
    );
    let _ = normal_sender.close().await;

    server_cancel.cancel();
    let _ = turn_server.close().await;
    Ok(())
}

#[tokio::test]
async fn shared_close_failure_preserves_send_failure_class_for_waiters() {
    let handle = tokio::spawn(std::future::pending::<()>());
    handle.abort();
    let join_err = handle.await.unwrap_err();
    let fatal_cases = [
        Error::NativeConnectionCloseTask(join_err),
        Error::NativeSendRuntimeUnavailable,
        Error::NativeConnectionRetirementTimeout { timeout_ms: 5_000 },
        Error::ConnectionReleased("peer".to_string()),
    ];
    for error in fatal_cases {
        let shared = SharedCloseFailure::from_error(error);
        let first = shared.clone().into_error();
        let second = shared.into_error();
        assert_eq!(first.send_failure_class(), TransportSendFailureClass::Fatal);
        assert_eq!(
            second.send_failure_class(),
            TransportSendFailureClass::Fatal
        );
    }

    let webrtc_shared = SharedCloseFailure::from_error(Error::Webrtc(webrtc::error::Error::new(
        "sctp closed".into(),
    )));
    assert_eq!(
        webrtc_shared.clone().into_error().send_failure_class(),
        TransportSendFailureClass::Ambiguous
    );
    assert_eq!(
        webrtc_shared.into_error().send_failure_class(),
        TransportSendFailureClass::Ambiguous
    );
}

#[tokio::test]
async fn turn_tcp_bridge_refuses_to_spawn_new_tcp_session_while_draining() -> Result<()> {
    use std::net::Ipv4Addr;
    use std::net::SocketAddr;

    use tokio::net::TcpListener;
    use tokio::net::UdpSocket;
    use turn_tcp::TurnBridgePhase;
    use webrtc::util::vnet::net::Net;

    assert!(turn_tcp::may_spawn_turn_tcp_session(
        TurnBridgePhase::Active
    ));
    assert!(turn_tcp::may_spawn_turn_tcp_session(
        TurnBridgePhase::Quiescing
    ));
    assert!(!turn_tcp::may_spawn_turn_tcp_session(
        TurnBridgePhase::Draining
    ));
    assert!(!turn_tcp::may_spawn_turn_tcp_session(
        TurnBridgePhase::Cancelled
    ));

    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let tcp_port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let accept_cancel = CancellationToken::new();
    let accept_task = tokio::spawn({
        let accepted = Arc::clone(&accepted);
        let cancel = accept_cancel.clone();
        async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    Ok((_sock, _)) = listener.accept() => {
                        accepted.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        }
    });

    let net = Arc::new(Net::new(None));
    let bridge_conn = net
        .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .map_err(|e| Error::Webrtc(webrtc::error::Error::Util(e)))?;
    let bridge_addr = bridge_conn
        .local_addr()
        .map_err(|e| Error::Webrtc(webrtc::error::Error::Util(e)))?;

    let cancel = CancellationToken::new();
    let drain_cancel = cancel.child_token();
    let inbound_cancel = drain_cancel.child_token();
    inbound_cancel.cancel();
    drain_cancel.cancel();

    let handle = turn_tcp::spawn_turn_tcp_bridge(
        bridge_conn,
        turn_tcp::TurnTcpEndpoint {
            host: "127.0.0.1".to_string(),
            port: tcp_port,
            use_tls: false,
        },
        inbound_cancel,
        drain_cancel,
        cancel.clone(),
    );

    let sender = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    // Valid TURN ChannelData datagram (channel 0x4000, len 4) with no existing session.
    let channel_data = [0x40, 0x00, 0x00, 0x04, 1, 2, 3, 4];
    sender.send_to(&channel_data, bridge_addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    assert_eq!(
        accepted.load(Ordering::SeqCst),
        0,
        "draining bridge must not initiate new TCP connects for unmapped UDP peers"
    );

    cancel.cancel();
    let _ = handle.await;
    accept_cancel.cancel();
    let _ = accept_task.await;
    Ok(())
}
