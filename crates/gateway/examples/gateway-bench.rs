//! Reproducible packet-to-stream gateway microbenchmark.
//!
//! This intentionally uses in-memory `PacketIo` and an echoing `OnionStreamConnector`: it
//! measures the shared TCP reconstruction and bridge path without claiming OS TUN or network
//! Onion performance. Platform smoke tests and the node's ignored public-route integration test
//! cover those separate boundaries.

use std::io;
use std::net::Ipv4Addr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use rings_gateway::BoxGatewayDuplex;
use rings_gateway::FlowId;
use rings_gateway::GatewayConfig;
use rings_gateway::GatewayError;
use rings_gateway::GatewayPlan;
use rings_gateway::GatewayRuntime;
use rings_gateway::Mtu;
use rings_gateway::OnionStreamConnector;
use rings_gateway::PacketIo;
use rings_gateway::PacketIoError;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv4Repr;
use smoltcp::wire::TcpControl;
use smoltcp::wire::TcpPacket;
use smoltcp::wire::TcpRepr;
use smoltcp::wire::TcpSeqNumber;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

const DEFAULT_FLOWS: usize = 256;
const DEFAULT_BYTES_PER_FLOW: usize = 1_024;
const MAX_FLOWS: usize = 30_000;
const MAX_BYTES_PER_FLOW: usize = 1_200;
const BASE_CLIENT_PORT: u16 = 20_000;
const TARGET_PORT: u16 = 443;
const TARGET: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 254);

struct ChannelPacketIo {
    ingress: mpsc::Receiver<Vec<u8>>,
    egress: mpsc::Sender<Vec<u8>>,
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
            PacketIoError::Write(io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))
        })
    }
}

struct EchoConnector;

#[async_trait::async_trait]
impl OnionStreamConnector for EchoConnector {
    async fn open_stream(
        &self,
        _flow: FlowId,
        stream: BoxGatewayDuplex,
    ) -> Result<(), GatewayError> {
        tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(stream);
            let _ = tokio::io::copy(&mut reader, &mut writer).await;
            let _ = writer.shutdown().await;
        });
        Ok(())
    }
}

struct TcpObservation {
    destination_port: u16,
    sequence: TcpSeqNumber,
    acknowledgment: Option<TcpSeqNumber>,
    rst: bool,
    payload: Vec<u8>,
}

impl TcpObservation {
    fn parse(packet: &[u8]) -> Result<Self, io::Error> {
        let ipv4 =
            Ipv4Packet::new_checked(packet).map_err(|error| io::Error::other(error.to_string()))?;
        let tcp = TcpPacket::new_checked(ipv4.payload())
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self {
            destination_port: tcp.dst_port(),
            sequence: tcp.seq_number(),
            acknowledgment: tcp.ack().then(|| tcp.ack_number()),
            rst: tcp.rst(),
            payload: tcp.payload().to_vec(),
        })
    }
}

fn config(flow_count: usize) -> Result<GatewayConfig, GatewayError> {
    let config = GatewayConfig {
        plan: GatewayPlan {
            addresses: vec!["100.64.0.1/32".parse::<ipnet::IpNet>().map_err(|error| {
                GatewayError::Platform {
                    operation: "benchmark-config",
                    message: error.to_string(),
                }
            })?],
            included_routes: vec!["203.0.113.254/32"
                .parse::<ipnet::IpNet>()
                .map_err(|error| GatewayError::Platform {
                    operation: "benchmark-config",
                    message: error.to_string(),
                })?],
            mtu: Mtu::try_from(1_500)?,
        },
        max_flows: flow_count,
        flow_idle_timeout: Duration::from_secs(120),
        tcp_buffer_bytes: 64 * 1_024,
    };
    config.validate()?;
    Ok(config)
}

fn client_packet(
    source_port: u16,
    control: TcpControl,
    sequence: TcpSeqNumber,
    acknowledgment: Option<TcpSeqNumber>,
    payload: &[u8],
) -> Result<Vec<u8>, io::Error> {
    let source = Ipv4Addr::new(100, 64, 0, 2);
    let tcp = TcpRepr {
        src_port: source_port,
        dst_port: TARGET_PORT,
        control,
        seq_number: sequence,
        ack_number: acknowledgment,
        window_len: 65_535,
        window_scale: None,
        max_seg_size: Some(1_200),
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload,
    };
    let ipv4 = Ipv4Repr {
        src_addr: source,
        dst_addr: TARGET,
        next_header: IpProtocol::Tcp,
        payload_len: tcp.buffer_len(),
        hop_limit: 64,
    };
    let mut packet = vec![0_u8; ipv4.buffer_len() + tcp.buffer_len()];
    ipv4.emit(
        &mut Ipv4Packet::new_unchecked(&mut packet),
        &ChecksumCapabilities::default(),
    );
    let Some(tcp_packet) = packet.get_mut(ipv4.buffer_len()..) else {
        return Err(io::Error::other(
            "benchmark packet buffer omitted its TCP payload",
        ));
    };
    tcp.emit(
        &mut TcpPacket::new_unchecked(tcp_packet),
        &source.into(),
        &TARGET.into(),
        &ChecksumCapabilities::default(),
    );
    Ok(packet)
}

async fn receive_for_port(
    egress: &mut mpsc::Receiver<Vec<u8>>,
    destination_port: u16,
    predicate: impl Fn(&TcpObservation) -> bool,
) -> Result<TcpObservation, io::Error> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let packet = egress.recv().await.ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "gateway egress closed")
            })?;
            let observation = TcpObservation::parse(&packet)?;
            if observation.destination_port == destination_port && predicate(&observation) {
                return Ok(observation);
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "gateway packet deadline"))?
}

fn parameter(name: &str, default: usize, maximum: usize) -> Result<usize, io::Error> {
    let value = match std::env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?,
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                error.to_string(),
            ))
        }
    };
    if value == 0 || value > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be in 1..={maximum}"),
        ));
    }
    Ok(value)
}

fn client_port(index: usize) -> Result<u16, io::Error> {
    let offset = u16::try_from(index)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    BASE_CLIENT_PORT.checked_add(offset).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "benchmark flow count exhausts client ports",
        )
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let flow_count = parameter("RINGS_GATEWAY_BENCH_FLOWS", DEFAULT_FLOWS, MAX_FLOWS)?;
    let bytes_per_flow = parameter(
        "RINGS_GATEWAY_BENCH_BYTES_PER_FLOW",
        DEFAULT_BYTES_PER_FLOW,
        MAX_BYTES_PER_FLOW,
    )?;

    let setup_started = Instant::now();
    let mut runtime = GatewayRuntime::new(config(flow_count)?, Arc::new(EchoConnector), 29)?;
    runtime.activate("memory-benchmark".to_string())?;
    let status = runtime.status_handle();
    let runtime_setup = setup_started.elapsed();

    let channel_capacity = flow_count.saturating_mul(4).max(16);
    let (ingress_tx, ingress_rx) = mpsc::channel(channel_capacity);
    let (egress_tx, mut egress_rx) = mpsc::channel(channel_capacity);
    let stop = Arc::new(AtomicBool::new(false));
    let task_stop = Arc::clone(&stop);
    let task = tokio::spawn(async move {
        let mut device = ChannelPacketIo {
            ingress: ingress_rx,
            egress: egress_tx,
        };
        runtime
            .run(&mut device, || task_stop.load(Ordering::Acquire))
            .await
    });

    let transfer_started = Instant::now();
    let mut first_flow_setup = Duration::ZERO;
    for index in 0..flow_count {
        let port = client_port(index)?;
        let flow_started = Instant::now();
        ingress_tx
            .send(client_packet(
                port,
                TcpControl::Syn,
                TcpSeqNumber(7),
                None,
                &[],
            )?)
            .await?;
        let syn_ack = receive_for_port(&mut egress_rx, port, |packet| {
            packet.acknowledgment.is_some() && packet.payload.is_empty() && !packet.rst
        })
        .await?;
        if index == 0 {
            first_flow_setup = flow_started.elapsed();
        }
        let server_sequence = syn_ack.sequence + 1;
        ingress_tx
            .send(client_packet(
                port,
                TcpControl::None,
                TcpSeqNumber(8),
                Some(server_sequence),
                &[],
            )?)
            .await?;

        let fill = u8::try_from(index % 251)?;
        let payload = vec![fill; bytes_per_flow];
        ingress_tx
            .send(client_packet(
                port,
                TcpControl::None,
                TcpSeqNumber(8),
                Some(server_sequence),
                payload.as_slice(),
            )?)
            .await?;

        let response = receive_for_port(&mut egress_rx, port, |packet| {
            packet.rst || !packet.payload.is_empty()
        })
        .await?;
        if response.rst {
            return Err(io::Error::other("gateway reset a benchmark flow").into());
        }
        if response.sequence != server_sequence || response.payload != payload {
            return Err(io::Error::other("gateway benchmark payload mismatch").into());
        }
        ingress_tx
            .send(client_packet(
                port,
                TcpControl::None,
                TcpSeqNumber(8) + bytes_per_flow,
                Some(response.sequence + response.payload.len()),
                &[],
            )?)
            .await?;
    }
    let transfer_elapsed = transfer_started.elapsed();
    let snapshot = status.snapshot();
    if snapshot.active_flows != flow_count {
        return Err(io::Error::other(format!(
            "expected {flow_count} active flows, observed {}",
            snapshot.active_flows
        ))
        .into());
    }

    stop.store(true, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "gateway stop deadline"))???;

    let payload_bytes = flow_count.saturating_mul(bytes_per_flow);
    let mib_per_second = payload_bytes as f64 / (1024.0 * 1024.0) / transfer_elapsed.as_secs_f64();
    println!(
        "{{\"runtime_setup_ms\":{:.3},\"first_flow_setup_ms\":{:.3},\"flow_count\":{},\"bytes_per_flow\":{},\"active_flows_peak\":{},\"payload_bytes\":{},\"transfer_seconds\":{:.6},\"application_mib_per_second\":{:.3}}}",
        runtime_setup.as_secs_f64() * 1_000.0,
        first_flow_setup.as_secs_f64() * 1_000.0,
        flow_count,
        bytes_per_flow,
        snapshot.active_flows,
        payload_bytes,
        transfer_elapsed.as_secs_f64(),
        mib_per_second,
    );
    Ok(())
}
