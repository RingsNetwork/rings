//! Packet-device and fail-closed IPv4/TCP parser boundaries.

use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;

use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::TcpPacket;

use crate::FlowId;
use crate::PacketIoError;
use crate::PacketParseError;

/// Raw IPv4 packet input and injection boundary.
///
/// Implementations return exactly one IP packet per `read_packet` call. The buffer contains no
/// platform-specific packet-information header.
#[async_trait::async_trait]
pub trait PacketIo: Send {
    /// Read one captured packet into `packet`, returning its initialized length.
    async fn read_packet(&mut self, packet: &mut [u8]) -> Result<usize, PacketIoError>;

    /// Inject one complete IP packet into the host network stack.
    async fn write_packet(&mut self, packet: &[u8]) -> Result<(), PacketIoError>;
}

/// Metadata extracted from one validated IPv4 TCP segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcpSegment {
    /// Immutable captured flow identity.
    pub flow: FlowId,
    /// Whether SYN is set.
    pub syn: bool,
    /// Whether ACK is set.
    pub ack: bool,
    /// Whether FIN is set.
    pub fin: bool,
    /// Whether RST is set.
    pub rst: bool,
    /// TCP payload length in bytes.
    pub payload_len: usize,
}

impl TcpSegment {
    /// Return whether this segment requests a new connection rather than acknowledging one.
    pub const fn opens_flow(self) -> bool {
        self.syn && !self.ack
    }
}

/// Explicit handling class for a validated IPv4 packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketDisposition {
    /// Feed this segment into the shared TCP stack.
    Tcp(TcpSegment),
    /// Drop UDP. DNS routing policy is owned by the platform plan; captured UDP never falls back.
    DropUdp,
    /// Drop an IP version or IPv4 protocol outside the IPv4/TCP milestone.
    DropUnsupportedProtocol,
}

/// Validate and classify one raw packet from a platform `PacketIo` device.
///
/// This function performs no IO and never creates a direct connection. Unsupported transports are
/// explicitly dropped after capture. Platform routing must implement [`crate::DnsPolicy::Bypass`]
/// before packets reach this boundary; captured UDP is never silently reinjected for direct egress.
pub fn classify_ipv4_packet(packet: &[u8]) -> Result<PacketDisposition, PacketParseError> {
    match packet.first().map(|byte| byte >> 4) {
        Some(4) => {}
        Some(6) => return Ok(PacketDisposition::DropUnsupportedProtocol),
        Some(_) => return Err(PacketParseError::NonIpv4),
        None => return Err(PacketParseError::MalformedIpv4),
    }
    let ipv4 = Ipv4Packet::new_checked(packet).map_err(|_| PacketParseError::MalformedIpv4)?;
    if ipv4.more_frags() || ipv4.frag_offset() != 0 {
        return Err(PacketParseError::FragmentedIpv4);
    }
    match ipv4.next_header() {
        IpProtocol::Tcp => classify_tcp(&ipv4),
        IpProtocol::Udp => Ok(PacketDisposition::DropUdp),
        _ => Ok(PacketDisposition::DropUnsupportedProtocol),
    }
}

fn classify_tcp(ipv4: &Ipv4Packet<&[u8]>) -> Result<PacketDisposition, PacketParseError> {
    let tcp = TcpPacket::new_checked(ipv4.payload()).map_err(|_| PacketParseError::MalformedTcp)?;
    if tcp.src_port() == 0 || tcp.dst_port() == 0 {
        return Err(PacketParseError::ZeroTcpPort);
    }
    let source_ip = Ipv4Addr::from(ipv4.src_addr().octets());
    let target_ip = Ipv4Addr::from(ipv4.dst_addr().octets());
    Ok(PacketDisposition::Tcp(TcpSegment {
        flow: FlowId {
            source: SocketAddr::V4(SocketAddrV4::new(source_ip, tcp.src_port())),
            target: SocketAddr::V4(SocketAddrV4::new(target_ip, tcp.dst_port())),
        },
        syn: tcp.syn(),
        ack: tcp.ack(),
        fin: tcp.fin(),
        rst: tcp.rst(),
        payload_len: tcp.payload().len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IPV4_HEADER_LEN: usize = 20;
    const TCP_HEADER_LEN: usize = 20;

    fn ipv4_packet(protocol: u8, transport_header: &[u8]) -> Vec<u8> {
        let total_len = IPV4_HEADER_LEN + transport_header.len();
        let mut packet = vec![0_u8; total_len];
        packet[0] = 0x45;
        packet[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
        packet[8] = 64;
        packet[9] = protocol;
        packet[12..16].copy_from_slice(&[100, 64, 0, 2]);
        packet[16..20].copy_from_slice(&[93, 184, 216, 34]);
        packet[IPV4_HEADER_LEN..].copy_from_slice(transport_header);
        packet
    }

    fn tcp_segment(flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut segment = vec![0_u8; TCP_HEADER_LEN + payload.len()];
        segment[0..2].copy_from_slice(&41_000_u16.to_be_bytes());
        segment[2..4].copy_from_slice(&443_u16.to_be_bytes());
        segment[12] = 5 << 4;
        segment[13] = flags;
        segment[TCP_HEADER_LEN..].copy_from_slice(payload);
        segment
    }

    #[test]
    fn initial_syn_binds_one_immutable_flow_target() {
        let packet = ipv4_packet(6, &tcp_segment(0x02, &[]));
        let disposition = classify_ipv4_packet(&packet).expect("valid SYN packet");
        let PacketDisposition::Tcp(segment) = disposition else {
            panic!("SYN must be classified as TCP");
        };
        assert!(segment.opens_flow());
        assert_eq!(segment.flow.source, "100.64.0.2:41000".parse().unwrap());
        assert_eq!(segment.flow.target, "93.184.216.34:443".parse().unwrap());
    }

    #[test]
    fn payload_and_terminal_flags_are_preserved() {
        let packet = ipv4_packet(6, &tcp_segment(0x11, b"hello"));
        let disposition = classify_ipv4_packet(&packet).expect("valid FIN/ACK packet");
        assert!(matches!(
            disposition,
            PacketDisposition::Tcp(TcpSegment {
                ack: true,
                fin: true,
                payload_len: 5,
                ..
            })
        ));
    }

    #[test]
    fn captured_udp_is_explicitly_dropped() {
        let packet = ipv4_packet(17, &[0_u8; 8]);
        assert_eq!(
            classify_ipv4_packet(&packet),
            Ok(PacketDisposition::DropUdp)
        );
    }

    #[test]
    fn captured_ipv6_is_explicitly_dropped() {
        assert_eq!(
            classify_ipv4_packet(&[0x60]),
            Ok(PacketDisposition::DropUnsupportedProtocol)
        );
    }

    #[test]
    fn fragmented_ipv4_fails_closed() {
        let mut packet = ipv4_packet(6, &tcp_segment(0x02, &[]));
        packet[6] = 0x20;
        assert_eq!(
            classify_ipv4_packet(&packet),
            Err(PacketParseError::FragmentedIpv4)
        );
    }

    #[test]
    fn malformed_transport_never_creates_a_flow() {
        let packet = ipv4_packet(6, &[0_u8; 4]);
        assert_eq!(
            classify_ipv4_packet(&packet),
            Err(PacketParseError::MalformedTcp)
        );
    }
}
