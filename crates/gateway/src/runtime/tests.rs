use std::net::Ipv4Addr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use ipnet::IpNet;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv4Repr;
use smoltcp::wire::TcpControl;
use smoltcp::wire::TcpPacket;
use smoltcp::wire::TcpRepr;
use smoltcp::wire::TcpSeqNumber;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use super::*;
use crate::BoxGatewayDuplex;
use crate::DnsPolicy;
use crate::GatewayHealth;
use crate::GatewayPlan;
use crate::Mtu;
use crate::RoutingMode;

struct ChannelPacketIo {
    ingress: mpsc::Receiver<Vec<u8>>,
    egress: mpsc::Sender<Vec<u8>>,
}

struct FailingPacketIo;

#[async_trait::async_trait]
impl PacketIo for FailingPacketIo {
    async fn read_packet(&mut self, _output: &mut [u8]) -> Result<usize, PacketIoError> {
        Err(PacketIoError::Closed)
    }

    async fn write_packet(&mut self, _packet: &[u8]) -> Result<(), PacketIoError> {
        Err(PacketIoError::Write(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "test cleanup write failure",
        )))
    }
}

#[async_trait::async_trait]
impl PacketIo for ChannelPacketIo {
    async fn read_packet(&mut self, output: &mut [u8]) -> Result<usize, PacketIoError> {
        let packet = self.ingress.recv().await.ok_or(PacketIoError::Closed)?;
        let capacity = output.len();
        let Some(destination) = output.get_mut(..packet.len()) else {
            return Err(PacketIoError::InvalidLength {
                length: packet.len(),
                capacity,
            });
        };
        destination.copy_from_slice(&packet);
        Ok(packet.len())
    }

    async fn write_packet(&mut self, packet: &[u8]) -> Result<(), PacketIoError> {
        self.egress.send(packet.to_vec()).await.map_err(|error| {
            PacketIoError::Write(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                error.to_string(),
            ))
        })
    }
}

struct RecordingConnector {
    opened: mpsc::Sender<FlowId>,
}

#[async_trait::async_trait]
impl OnionStreamConnector for RecordingConnector {
    async fn open_stream(
        &self,
        flow: FlowId,
        mut stream: BoxGatewayDuplex,
    ) -> Result<(), GatewayError> {
        self.opened
            .send(flow)
            .await
            .map_err(|error| GatewayError::OnionUnavailable {
                target: flow.target,
                message: error.to_string(),
            })?;
        tokio::spawn(async move {
            let mut buffer = [0_u8; 64];
            while matches!(stream.read(&mut buffer).await, Ok(length) if length > 0) {}
        });
        Ok(())
    }
}

struct EchoConnector {
    opened: mpsc::Sender<FlowId>,
}

#[async_trait::async_trait]
impl OnionStreamConnector for EchoConnector {
    async fn open_stream(
        &self,
        flow: FlowId,
        mut stream: BoxGatewayDuplex,
    ) -> Result<(), GatewayError> {
        self.opened
            .send(flow)
            .await
            .map_err(|error| GatewayError::OnionUnavailable {
                target: flow.target,
                message: error.to_string(),
            })?;
        tokio::spawn(async move {
            let mut buffer = [0_u8; 64];
            loop {
                match stream.read(&mut buffer).await {
                    Ok(0) | Err(_) => {
                        let _ = stream.shutdown().await;
                        break;
                    }
                    Ok(length) => {
                        let Some(bytes) = buffer.get(..length) else {
                            break;
                        };
                        if stream.write_all(bytes).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(())
    }
}

struct FailingConnector {
    attempted: mpsc::Sender<FlowId>,
}

#[async_trait::async_trait]
impl OnionStreamConnector for FailingConnector {
    async fn open_stream(
        &self,
        flow: FlowId,
        _stream: BoxGatewayDuplex,
    ) -> Result<(), GatewayError> {
        self.attempted
            .send(flow)
            .await
            .map_err(|error| GatewayError::OnionUnavailable {
                target: flow.target,
                message: error.to_string(),
            })?;
        Err(GatewayError::OnionUnavailable {
            target: flow.target,
            message: "test route unavailable".to_string(),
        })
    }
}

struct TcpObservation {
    sequence: TcpSeqNumber,
    acknowledgment: Option<TcpSeqNumber>,
    fin: bool,
    rst: bool,
    payload: Vec<u8>,
}

impl TcpObservation {
    fn parse(packet: &[u8]) -> Self {
        let ipv4 = Ipv4Packet::new_checked(packet).expect("valid egress IPv4 packet");
        let tcp = TcpPacket::new_checked(ipv4.payload()).expect("valid egress TCP segment");
        Self {
            sequence: tcp.seq_number(),
            acknowledgment: tcp.ack().then(|| tcp.ack_number()),
            fin: tcp.fin(),
            rst: tcp.rst(),
            payload: tcp.payload().to_vec(),
        }
    }
}

async fn receive_matching_tcp(
    egress: &mut mpsc::Receiver<Vec<u8>>,
    matches: impl Fn(&TcpObservation) -> bool,
) -> TcpObservation {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let packet = egress.recv().await.expect("gateway egress remains open");
            let observation = TcpObservation::parse(&packet);
            if matches(&observation) {
                return observation;
            }
        }
    })
    .await
    .expect("matching TCP packet deadline")
}

fn route(address: Ipv4Addr, prefix: u8) -> IpNet {
    IpNet::new(address.into(), prefix).expect("valid test network")
}

fn config() -> GatewayConfig {
    GatewayConfig {
        plan: GatewayPlan {
            routing_mode: RoutingMode::Default,
            addresses: vec![route(Ipv4Addr::new(100, 64, 0, 1), 30)],
            included_routes: vec![route(Ipv4Addr::UNSPECIFIED, 0)],
            excluded_routes: vec![route(Ipv4Addr::LOCALHOST, 8)],
            mtu: Mtu::try_from(1_280).expect("valid test MTU"),
            dns_policy: DnsPolicy::Block,
            dns_servers: vec!["1.1.1.1".parse().expect("test DNS")],
        },
        max_flows: 8,
        flow_idle_timeout: Duration::from_secs(30),
        tcp_buffer_bytes: 16 * 1_024,
    }
}

fn client_packet(
    control: TcpControl,
    sequence: TcpSeqNumber,
    acknowledgment: Option<TcpSeqNumber>,
) -> Vec<u8> {
    client_packet_to_port(control, sequence, acknowledgment, 443)
}

fn client_packet_to_port(
    control: TcpControl,
    sequence: TcpSeqNumber,
    acknowledgment: Option<TcpSeqNumber>,
    target_port: u16,
) -> Vec<u8> {
    client_packet_with_payload(control, sequence, acknowledgment, target_port, &[])
}

fn client_packet_with_payload(
    control: TcpControl,
    sequence: TcpSeqNumber,
    acknowledgment: Option<TcpSeqNumber>,
    target_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let source = Ipv4Addr::new(100, 64, 0, 2);
    let target = Ipv4Addr::new(93, 184, 216, 34);
    let tcp = TcpRepr {
        src_port: 41_000,
        dst_port: target_port,
        control,
        seq_number: sequence,
        ack_number: acknowledgment,
        window_len: 8_192,
        window_scale: None,
        max_seg_size: Some(1_200),
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload,
    };
    let ipv4 = Ipv4Repr {
        src_addr: source,
        dst_addr: target,
        next_header: IpProtocol::Tcp,
        payload_len: tcp.buffer_len(),
        hop_limit: 64,
    };
    let mut packet = vec![0_u8; ipv4.buffer_len() + tcp.buffer_len()];
    ipv4.emit(
        &mut Ipv4Packet::new_unchecked(&mut packet),
        &ChecksumCapabilities::default(),
    );
    tcp.emit(
        &mut TcpPacket::new_unchecked(&mut packet[ipv4.buffer_len()..]),
        &source.into(),
        &target.into(),
        &ChecksumCapabilities::default(),
    );
    packet
}

#[test]
fn block_dns_policy_never_opens_a_tcp_53_flow() {
    let (opened_tx, _opened_rx) = mpsc::channel(1);
    let connector = Arc::new(RecordingConnector { opened: opened_tx });
    let mut runtime = GatewayRuntime::new(config(), connector, 7).expect("valid runtime");
    runtime
        .activate("memory-tun".to_string())
        .expect("activate runtime");

    runtime
        .ingest_packet(
            client_packet_to_port(TcpControl::Syn, TcpSeqNumber(7), None, 53),
            Duration::ZERO,
        )
        .expect("blocked DNS packet is handled");

    assert_eq!(runtime.status().active_flows, 0);
    assert!(runtime.tcp.take_egress().is_empty());
}

fn handshake_ack(syn_ack: &[u8]) -> Vec<u8> {
    let ipv4 = Ipv4Packet::new_checked(syn_ack).expect("valid SYN-ACK IPv4");
    let tcp = TcpPacket::new_checked(ipv4.payload()).expect("valid SYN-ACK TCP");
    client_packet(
        TcpControl::None,
        TcpSeqNumber(8),
        Some(tcp.seq_number() + 1),
    )
}

fn handshake_ack_from_observation(syn_ack: &TcpObservation) -> Vec<u8> {
    client_packet(
        TcpControl::None,
        TcpSeqNumber(8),
        Some(syn_ack.sequence + 1),
    )
}

#[tokio::test]
async fn runtime_opens_onion_only_after_tcp_handshake_and_stops_cleanly() {
    let (ingress_tx, ingress_rx) = mpsc::channel(4);
    let (egress_tx, mut egress_rx) = mpsc::channel(8);
    let (opened_tx, mut opened_rx) = mpsc::channel(1);
    let connector = Arc::new(RecordingConnector { opened: opened_tx });
    let mut runtime = GatewayRuntime::new(config(), connector, 7).expect("valid runtime");
    runtime
        .activate("memory-tun".to_string())
        .expect("activate runtime");
    runtime.set_exit_availability(ExitAvailability::Available, None);
    let stop = Arc::new(AtomicBool::new(false));
    let task_stop = Arc::clone(&stop);
    let task = tokio::spawn(async move {
        let mut device = ChannelPacketIo {
            ingress: ingress_rx,
            egress: egress_tx,
        };
        let result = runtime
            .run(&mut device, || task_stop.load(Ordering::Acquire))
            .await;
        (runtime, result)
    });

    ingress_tx
        .send(client_packet(TcpControl::Syn, TcpSeqNumber(7), None))
        .await
        .expect("send SYN");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), opened_rx.recv())
            .await
            .is_err()
    );
    let syn_ack = tokio::time::timeout(Duration::from_secs(1), egress_rx.recv())
        .await
        .expect("SYN-ACK deadline")
        .expect("SYN-ACK packet");
    ingress_tx
        .send(handshake_ack(&syn_ack))
        .await
        .expect("send handshake ACK");
    let opened = tokio::time::timeout(Duration::from_secs(1), opened_rx.recv())
        .await
        .expect("Onion open deadline")
        .expect("Onion open event");
    assert_eq!(opened.target, "93.184.216.34:443".parse().unwrap());

    stop.store(true, Ordering::Release);
    let (runtime, result) = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("runtime stop deadline")
        .expect("runtime task");
    result.expect("clean runtime stop");
    assert_eq!(runtime.status().health, GatewayHealth::Inactive);
    assert_eq!(runtime.status().active_flows, 0);
}

#[tokio::test]
async fn runtime_preserves_primary_and_cleanup_errors_while_finishing_stop() {
    let (opened_tx, _opened_rx) = mpsc::channel(1);
    let connector = Arc::new(RecordingConnector { opened: opened_tx });
    let mut runtime = GatewayRuntime::new(config(), connector, 19).expect("valid runtime");
    runtime
        .activate("memory-tun".to_string())
        .expect("activate runtime");
    runtime
        .ingest_packet(
            client_packet(TcpControl::Syn, TcpSeqNumber(7), None),
            Duration::ZERO,
        )
        .expect("capture flow and queue SYN-ACK");

    let error = runtime
        .run(&mut FailingPacketIo, || false)
        .await
        .expect_err("runtime and cleanup must both fail");

    assert!(matches!(
        error,
        GatewayError::RuntimeCleanup {
            runtime,
            cleanup,
        } if matches!(*runtime, GatewayError::PacketIo(_))
            && matches!(*cleanup, GatewayError::PacketIo(_))
    ));
    assert_eq!(runtime.status().health, GatewayHealth::Inactive);
    assert_eq!(runtime.status().active_flows, 0);
    assert_eq!(runtime.tcp.flow_count(), 0);
}

#[tokio::test]
async fn runtime_bridges_tcp_payload_and_both_half_closes() {
    let (ingress_tx, ingress_rx) = mpsc::channel(16);
    let (egress_tx, mut egress_rx) = mpsc::channel(32);
    let (opened_tx, mut opened_rx) = mpsc::channel(1);
    let connector = Arc::new(EchoConnector { opened: opened_tx });
    let mut runtime = GatewayRuntime::new(config(), connector, 11).expect("valid runtime");
    runtime
        .activate("memory-tun".to_string())
        .expect("activate runtime");
    runtime.set_exit_availability(ExitAvailability::Available, None);
    let status = runtime.status_handle();
    let stop = Arc::new(AtomicBool::new(false));
    let task_stop = Arc::clone(&stop);
    let task = tokio::spawn(async move {
        let mut device = ChannelPacketIo {
            ingress: ingress_rx,
            egress: egress_tx,
        };
        let result = runtime
            .run(&mut device, || task_stop.load(Ordering::Acquire))
            .await;
        (runtime, result)
    });

    ingress_tx
        .send(client_packet(TcpControl::Syn, TcpSeqNumber(7), None))
        .await
        .expect("send SYN");
    let syn_ack = receive_matching_tcp(&mut egress_rx, |packet| {
        packet.acknowledgment.is_some() && packet.payload.is_empty() && !packet.fin
    })
    .await;
    let server_next = syn_ack.sequence + 1;
    ingress_tx
        .send(handshake_ack_from_observation(&syn_ack))
        .await
        .expect("send handshake ACK");
    let opened = tokio::time::timeout(Duration::from_secs(1), opened_rx.recv())
        .await
        .expect("Onion open deadline")
        .expect("Onion open event");
    assert_eq!(opened.target, "93.184.216.34:443".parse().unwrap());

    let payload = b"captured-through-onion";
    ingress_tx
        .send(client_packet_with_payload(
            TcpControl::None,
            TcpSeqNumber(8),
            Some(server_next),
            443,
            payload,
        ))
        .await
        .expect("send captured payload");
    let echoed = receive_matching_tcp(&mut egress_rx, |packet| !packet.payload.is_empty()).await;
    assert_eq!(echoed.payload, payload);
    let client_after_payload = TcpSeqNumber(8) + payload.len();
    let server_after_payload = echoed.sequence + echoed.payload.len();
    ingress_tx
        .send(client_packet(
            TcpControl::Fin,
            client_after_payload,
            Some(server_after_payload),
        ))
        .await
        .expect("send client FIN");
    let server_fin = receive_matching_tcp(&mut egress_rx, |packet| packet.fin).await;
    assert_eq!(server_fin.sequence, server_after_payload);
    ingress_tx
        .send(client_packet(
            TcpControl::None,
            client_after_payload + 1,
            Some(server_fin.sequence + 1),
        ))
        .await
        .expect("acknowledge server FIN");

    tokio::time::timeout(Duration::from_secs(1), async {
        while status.snapshot().active_flows != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("graceful flow cleanup deadline");
    stop.store(true, Ordering::Release);
    let (runtime, result) = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("runtime stop deadline")
        .expect("runtime task");
    result.expect("clean runtime stop");
    assert_eq!(runtime.status().health, GatewayHealth::Inactive);
}

#[tokio::test]
async fn onion_open_failure_resets_captured_flow_without_fallback() {
    let (ingress_tx, ingress_rx) = mpsc::channel(8);
    let (egress_tx, mut egress_rx) = mpsc::channel(16);
    let (attempted_tx, mut attempted_rx) = mpsc::channel(1);
    let connector = Arc::new(FailingConnector {
        attempted: attempted_tx,
    });
    let mut runtime = GatewayRuntime::new(config(), connector, 13).expect("valid runtime");
    runtime
        .activate("memory-tun".to_string())
        .expect("activate runtime");
    let status = runtime.status_handle();
    let stop = Arc::new(AtomicBool::new(false));
    let task_stop = Arc::clone(&stop);
    let task = tokio::spawn(async move {
        let mut device = ChannelPacketIo {
            ingress: ingress_rx,
            egress: egress_tx,
        };
        let result = runtime
            .run(&mut device, || task_stop.load(Ordering::Acquire))
            .await;
        (runtime, result)
    });

    ingress_tx
        .send(client_packet(TcpControl::Syn, TcpSeqNumber(7), None))
        .await
        .expect("send SYN");
    let syn_ack = receive_matching_tcp(&mut egress_rx, |packet| {
        packet.acknowledgment.is_some() && packet.payload.is_empty() && !packet.fin
    })
    .await;
    ingress_tx
        .send(handshake_ack_from_observation(&syn_ack))
        .await
        .expect("send handshake ACK");
    let attempted = tokio::time::timeout(Duration::from_secs(1), attempted_rx.recv())
        .await
        .expect("Onion attempt deadline")
        .expect("Onion attempt event");
    assert_eq!(attempted.target, "93.184.216.34:443".parse().unwrap());
    let reset = receive_matching_tcp(&mut egress_rx, |packet| packet.rst).await;
    assert!(reset.rst);

    tokio::time::timeout(Duration::from_secs(1), async {
        while status.snapshot().active_flows != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("failed flow cleanup deadline");
    let failed = status.snapshot();
    assert_eq!(failed.health, GatewayHealth::Degraded);
    assert!(failed
        .reason
        .as_deref()
        .is_some_and(|reason| reason.contains("test route unavailable")));
    stop.store(true, Ordering::Release);
    let (runtime, result) = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("runtime stop deadline")
        .expect("runtime task");
    result.expect("clean runtime stop");
    assert_eq!(runtime.status().active_flows, 0);
}

#[test]
fn late_bridge_events_for_a_released_flow_are_idempotent() {
    let (opened_tx, _opened_rx) = mpsc::channel(1);
    let connector = Arc::new(RecordingConnector { opened: opened_tx });
    let mut runtime = GatewayRuntime::new(config(), connector, 17).expect("valid runtime");
    runtime
        .activate("memory-tun".to_string())
        .expect("activate runtime");
    let packet = client_packet(TcpControl::Syn, TcpSeqNumber(7), None);
    let flow = match crate::classify_ipv4_packet(&packet).expect("valid SYN") {
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

    assert_eq!(runtime.status().active_flows, 0);
    assert_eq!(runtime.status().reason, None);
}
