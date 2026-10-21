//! In-memory packet fixtures for driving a [`GatewayRuntime`](crate::GatewayRuntime) without an
//! operating-system tunnel.
//!
//! The gateway's own tests, its benchmark and the node's gateway integration test share these:
//! a [`ChannelPacketIo`] device over two channels, a [`ClientPath`] that emits the captured
//! client's IPv4/TCP segments, and [`receive_matching_tcp`], which reads the gateway's egress
//! as [`TcpObservation`]s until one satisfies a predicate. Every fixture reports a malformed
//! packet, a closed channel or a missed deadline as an [`io::Error`], so a caller decides how a
//! test fails.

use std::io;
#[cfg(test)]
use std::net::Ipv4Addr;
use std::net::SocketAddrV4;
use std::time::Duration;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv4Repr;
use smoltcp::wire::TcpControl;
use smoltcp::wire::TcpPacket;
use smoltcp::wire::TcpRepr;
use smoltcp::wire::TcpSeqNumber;
use tokio::sync::mpsc;

use crate::PacketIo;
use crate::PacketIoError;

/// A packet device whose reads drain `ingress` and whose writes feed `egress`.
pub struct ChannelPacketIo {
    /// Packets the gateway reads, as if captured from the tunnel.
    pub ingress: mpsc::Receiver<Vec<u8>>,
    /// Packets the gateway writes back towards the captured client.
    pub egress: mpsc::Sender<Vec<u8>>,
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

/// The TCP header fields and payload of one IPv4 packet the gateway emitted.
pub struct TcpObservation {
    /// The client port the segment is addressed to.
    pub destination_port: u16,
    /// The segment's sequence number.
    pub sequence: TcpSeqNumber,
    /// The acknowledged sequence number, when the segment carries ACK.
    pub acknowledgment: Option<TcpSeqNumber>,
    /// Whether the segment carries FIN.
    pub fin: bool,
    /// Whether the segment carries RST.
    pub rst: bool,
    /// The segment's payload bytes.
    pub payload: Vec<u8>,
}

impl TcpObservation {
    /// Parse `packet` as an IPv4 packet carrying a TCP segment.
    pub fn parse(packet: &[u8]) -> io::Result<Self> {
        let ipv4 = Ipv4Packet::new_checked(packet).map_err(io::Error::other)?;
        let tcp = TcpPacket::new_checked(ipv4.payload()).map_err(io::Error::other)?;
        Ok(Self {
            destination_port: tcp.dst_port(),
            sequence: tcp.seq_number(),
            acknowledgment: tcp.ack().then(|| tcp.ack_number()),
            fin: tcp.fin(),
            rst: tcp.rst(),
            payload: tcp.payload().to_vec(),
        })
    }
}

/// The captured client's side of one TCP connection: its endpoints and advertised window.
#[derive(Clone, Copy, Debug)]
pub struct ClientPath {
    /// The client's address and port inside the tunnel.
    pub source: SocketAddrV4,
    /// The captured destination the client dials.
    pub target: SocketAddrV4,
    /// The receive window the client advertises.
    pub window_len: u16,
}

impl ClientPath {
    /// Emit one client segment on this path as a checksummed IPv4 packet, advertising an MSS of
    /// 1200 bytes and no TCP options beyond it.
    pub fn packet(
        &self,
        control: TcpControl,
        sequence: TcpSeqNumber,
        acknowledgment: Option<TcpSeqNumber>,
        payload: &[u8],
    ) -> io::Result<Vec<u8>> {
        let source = *self.source.ip();
        let target = *self.target.ip();
        let tcp = TcpRepr {
            src_port: self.source.port(),
            dst_port: self.target.port(),
            control,
            seq_number: sequence,
            ack_number: acknowledgment,
            window_len: self.window_len,
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
        let tcp_packet = packet
            .get_mut(ipv4.buffer_len()..)
            .ok_or_else(|| io::Error::other("client packet buffer omitted its TCP segment"))?;
        tcp.emit(
            &mut TcpPacket::new_unchecked(tcp_packet),
            &source.into(),
            &target.into(),
            &ChecksumCapabilities::default(),
        );
        Ok(packet)
    }
}

/// The client path of the gateway's unit tests: `100.64.0.2:41000 → 93.184.216.34:443`,
/// advertising an 8 KiB window.
#[cfg(test)]
pub(crate) const UNIT_CLIENT: ClientPath = ClientPath {
    source: SocketAddrV4::new(Ipv4Addr::new(100, 64, 0, 2), 41_000),
    target: SocketAddrV4::new(Ipv4Addr::new(93, 184, 216, 34), 443),
    window_len: 8_192,
};

/// Read `egress` until a packet's [`TcpObservation`] satisfies `predicate`, within `deadline`.
///
/// Packets that do not satisfy it are discarded. A malformed packet, a closed channel or the
/// deadline ends the wait with an error.
pub async fn receive_matching_tcp(
    egress: &mut mpsc::Receiver<Vec<u8>>,
    deadline: Duration,
    predicate: impl Fn(&TcpObservation) -> bool,
) -> io::Result<TcpObservation> {
    tokio::time::timeout(deadline, async {
        loop {
            let packet = egress.recv().await.ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "gateway egress closed")
            })?;
            let observation = TcpObservation::parse(&packet)?;
            if predicate(&observation) {
                return Ok(observation);
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "gateway packet deadline"))?
}
