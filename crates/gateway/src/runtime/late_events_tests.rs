use super::*;

#[test]
fn late_bridge_events_for_a_released_flow_are_idempotent() {
    let (opened_tx, _opened_rx) = mpsc::channel(1);
    let connector = Arc::new(RecordingConnector { opened: opened_tx });
    let mut runtime = GatewayRuntime::new(config(), connector, 17).expect("valid runtime");
    runtime
        .activate("memory-tun".to_string())
        .expect("activate runtime");
    let packet = client_packet(TcpControl::Syn, TcpSeqNumber(7), None);
    let flow = match crate::classify_ipv4_packet(&packet) {
        PacketDisposition::Tcp(segment) => segment.flow,
        _ => panic!("SYN must be classified as TCP"),
    };

    runtime
        .handle_bridge_event(
            BridgeEvent::Data {
                flow,
                bytes: b"late".to_vec(),
                consumed: oneshot::channel().0,
            },
            Duration::ZERO,
        )
        .expect("late data is ignored");
    runtime
        .handle_bridge_event(
            BridgeEvent::ClientBuffer {
                flow,
                buffer: Vec::new(),
            },
            Duration::ZERO,
        )
        .expect("late returned buffer is ignored");
    runtime
        .handle_bridge_event(BridgeEvent::PeerClosed(flow), Duration::ZERO)
        .expect("late EOF is ignored");
    runtime
        .handle_bridge_event(
            BridgeEvent::Failed {
                flow,
                error: GatewayError::OnionUnavailable {
                    target: flow.target,
                    message: "late failure".to_string(),
                },
            },
            Duration::ZERO,
        )
        .expect("late failure is ignored");

    let status = runtime.status_handle().snapshot();
    assert_eq!(status.active_flows, 0);
    assert_eq!(
        status.reason.as_deref(),
        Some(crate::status::EXIT_DISCOVERY_PENDING)
    );
    assert_eq!(status.last_flow_reset, None);
}
