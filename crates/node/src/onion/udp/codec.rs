//! Binary frame codec for the world-facing `udp` datagram session (`onion::udp`).
//!
//! An `onion::udp` session carries framed control and datagram records over the ordered byte
//! stream of an `OnionClientStream` / `OnionExitSessions`:
//!
//! ```text
//! record     = len:u32_be ‖ body[len]                  (1 ≤ len ≤ MAX_UDP_FRAME_BODY_BYTES)
//! ip_addr    = 0x04 ‖ ipv4[4]  |  0x06 ‖ ipv6[16]
//! sock_addr  = ip_addr ‖ port:u16_be
//!
//! client → exit:
//!   0x01 ‖ ip_addr                                      AllowPeer { ip }
//!   0x02 ‖ sock_addr ‖ payload                          Datagram  { peer, payload }
//!
//! exit → client:
//!   0x01 ‖ sock_addr                                    Bound       { local_addr }
//!   0x02 ‖ sock_addr ‖ payload                          Datagram    { peer, payload }
//!   0x03 ‖ ip_addr                                      PeerAllowed { ip }
//! ```

use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;

use bytes::Buf;
use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;

use crate::error::Error;
use crate::error::Result;

/// Maximum UDP datagram payload bytes carried in one `onion::udp` record.
pub const MAX_UDP_DATAGRAM_BYTES: usize = 65_535;

/// Length prefix byte width (`u32` big-endian).
const FRAME_LEN_HEADER_BYTES: usize = 4;

/// Largest encoded `sock_addr` (`0x06` + 16-byte IPv6 + 2-byte port).
const MAX_SOCK_ADDR_BYTES: usize = 1 + 16 + 2;

/// Largest legal body length (`tag` + `sock_addr` + `MAX_UDP_DATAGRAM_BYTES`).
pub(crate) const MAX_UDP_FRAME_BODY_BYTES: usize = 1 + MAX_SOCK_ADDR_BYTES + MAX_UDP_DATAGRAM_BYTES;

/// Upper bound on partial/unconsumed bytes buffered inside one [`OnionUdpFrameDecoder`].
const MAX_DECODER_BUFFER_BYTES: usize = 4 * (FRAME_LEN_HEADER_BYTES + MAX_UDP_FRAME_BODY_BYTES);

const FAMILY_IPV4: u8 = 0x04;
const FAMILY_IPV6: u8 = 0x06;

const CLIENT_TAG_ALLOW_PEER: u8 = 0x01;
const CLIENT_TAG_DATAGRAM: u8 = 0x02;

const EXIT_TAG_BOUND: u8 = 0x01;
const EXIT_TAG_DATAGRAM: u8 = 0x02;
const EXIT_TAG_PEER_ALLOWED: u8 = 0x03;

/// Client-to-exit record of an `onion::udp` session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OnionUdpClientFrame {
    /// Open or refresh a stateful ingress/egress pinhole for `ip` at the exit socket.
    AllowPeer {
        /// Peer IP address permitted to exchange UDP datagrams with the exit socket.
        ip: IpAddr,
    },
    /// Send `payload` from the exit UDP socket to `peer` (refreshing `peer.ip()`'s pinhole).
    Datagram {
        /// Destination UDP socket address.
        peer: SocketAddr,
        /// Raw UDP datagram payload (`<= MAX_UDP_DATAGRAM_BYTES`).
        payload: Bytes,
    },
}

/// Exit-to-client record of an `onion::udp` session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OnionUdpExitFrame {
    /// Initial acknowledgement naming the concrete bound exit socket address `E_exit`.
    Bound {
        /// Externally reachable `(ip, port)` of the exit's UDP socket.
        local_addr: SocketAddr,
    },
    /// Inbound UDP datagram received on `E_exit` from `peer`.
    Datagram {
        /// Source UDP socket address observed at `E_exit`.
        peer: SocketAddr,
        /// Raw UDP datagram payload (`<= MAX_UDP_DATAGRAM_BYTES`).
        payload: Bytes,
    },
    /// Confirmation that `AllowPeer { ip }` has been installed at the exit socket.
    PeerAllowed {
        /// Peer IP address whose pinhole is now active at `E_exit`.
        ip: IpAddr,
    },
}

fn put_ip(buf: &mut BytesMut, ip: IpAddr) {
    match ip {
        IpAddr::V4(v4) => {
            buf.put_u8(FAMILY_IPV4);
            buf.put_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            buf.put_u8(FAMILY_IPV6);
            buf.put_slice(&v6.octets());
        }
    }
}

fn put_sock_addr(buf: &mut BytesMut, addr: SocketAddr) {
    put_ip(buf, addr.ip());
    buf.put_u16(addr.port());
}

fn parse_ip(bytes: &[u8]) -> Result<(IpAddr, &[u8])> {
    let (&family, rest) = bytes.split_first().ok_or(Error::InvalidData)?;
    match family {
        FAMILY_IPV4 => {
            if rest.len() < 4 {
                return Err(Error::InvalidData);
            }
            let (raw, tail) = rest.split_at(4);
            let octets: [u8; 4] = raw.try_into().map_err(|_| Error::InvalidData)?;
            Ok((IpAddr::V4(Ipv4Addr::from(octets)), tail))
        }
        FAMILY_IPV6 => {
            if rest.len() < 16 {
                return Err(Error::InvalidData);
            }
            let (raw, tail) = rest.split_at(16);
            let octets: [u8; 16] = raw.try_into().map_err(|_| Error::InvalidData)?;
            Ok((IpAddr::V6(Ipv6Addr::from(octets)), tail))
        }
        _ => Err(Error::InvalidData),
    }
}

fn parse_sock_addr(bytes: &[u8]) -> Result<(SocketAddr, &[u8])> {
    let (ip, rest) = parse_ip(bytes)?;
    if rest.len() < 2 {
        return Err(Error::InvalidData);
    }
    let (port_raw, tail) = rest.split_at(2);
    let port_bytes: [u8; 2] = port_raw.try_into().map_err(|_| Error::InvalidData)?;
    let port = u16::from_be_bytes(port_bytes);
    Ok((SocketAddr::new(ip, port), tail))
}

impl OnionUdpClientFrame {
    /// Encode this client-to-exit frame with its 4-byte big-endian length prefix.
    pub fn encode(&self) -> Result<Bytes> {
        match self {
            Self::AllowPeer { ip } => {
                let body_len = 1 + match ip {
                    IpAddr::V4(_) => 5,
                    IpAddr::V6(_) => 17,
                };
                let mut buf = BytesMut::with_capacity(FRAME_LEN_HEADER_BYTES + body_len);
                buf.put_u32(u32::try_from(body_len).map_err(|_| Error::InvalidData)?);
                buf.put_u8(CLIENT_TAG_ALLOW_PEER);
                put_ip(&mut buf, *ip);
                Ok(buf.freeze())
            }
            Self::Datagram { peer, payload } => {
                if payload.len() > MAX_UDP_DATAGRAM_BYTES {
                    return Err(Error::InvalidData);
                }
                let addr_len = match peer.ip() {
                    IpAddr::V4(_) => 7,
                    IpAddr::V6(_) => 19,
                };
                let body_len = 1 + addr_len + payload.len();
                let mut buf = BytesMut::with_capacity(FRAME_LEN_HEADER_BYTES + body_len);
                buf.put_u32(u32::try_from(body_len).map_err(|_| Error::InvalidData)?);
                buf.put_u8(CLIENT_TAG_DATAGRAM);
                put_sock_addr(&mut buf, *peer);
                buf.put_slice(payload);
                Ok(buf.freeze())
            }
        }
    }

    fn decode_body(body: Bytes) -> Result<Self> {
        let (&tag, rest) = body.split_first().ok_or(Error::InvalidData)?;
        match tag {
            CLIENT_TAG_ALLOW_PEER => {
                let (ip, tail) = parse_ip(rest)?;
                if !tail.is_empty() {
                    return Err(Error::InvalidData);
                }
                Ok(Self::AllowPeer { ip })
            }
            CLIENT_TAG_DATAGRAM => {
                let (peer, tail) = parse_sock_addr(rest)?;
                if tail.len() > MAX_UDP_DATAGRAM_BYTES {
                    return Err(Error::InvalidData);
                }
                let payload_offset = body.len() - tail.len();
                Ok(Self::Datagram {
                    peer,
                    payload: body.slice(payload_offset..),
                })
            }
            _ => Err(Error::InvalidData),
        }
    }
}

impl OnionUdpExitFrame {
    /// Encode this exit-to-client frame with its 4-byte big-endian length prefix.
    pub fn encode(&self) -> Result<Bytes> {
        match self {
            Self::Bound { local_addr } => {
                let addr_len = match local_addr.ip() {
                    IpAddr::V4(_) => 7,
                    IpAddr::V6(_) => 19,
                };
                let body_len = 1 + addr_len;
                let mut buf = BytesMut::with_capacity(FRAME_LEN_HEADER_BYTES + body_len);
                buf.put_u32(u32::try_from(body_len).map_err(|_| Error::InvalidData)?);
                buf.put_u8(EXIT_TAG_BOUND);
                put_sock_addr(&mut buf, *local_addr);
                Ok(buf.freeze())
            }
            Self::Datagram { peer, payload } => {
                if payload.len() > MAX_UDP_DATAGRAM_BYTES {
                    return Err(Error::InvalidData);
                }
                let addr_len = match peer.ip() {
                    IpAddr::V4(_) => 7,
                    IpAddr::V6(_) => 19,
                };
                let body_len = 1 + addr_len + payload.len();
                let mut buf = BytesMut::with_capacity(FRAME_LEN_HEADER_BYTES + body_len);
                buf.put_u32(u32::try_from(body_len).map_err(|_| Error::InvalidData)?);
                buf.put_u8(EXIT_TAG_DATAGRAM);
                put_sock_addr(&mut buf, *peer);
                buf.put_slice(payload);
                Ok(buf.freeze())
            }
            Self::PeerAllowed { ip } => {
                let ip_len = match ip {
                    IpAddr::V4(_) => 5,
                    IpAddr::V6(_) => 17,
                };
                let body_len = 1 + ip_len;
                let mut buf = BytesMut::with_capacity(FRAME_LEN_HEADER_BYTES + body_len);
                buf.put_u32(u32::try_from(body_len).map_err(|_| Error::InvalidData)?);
                buf.put_u8(EXIT_TAG_PEER_ALLOWED);
                put_ip(&mut buf, *ip);
                Ok(buf.freeze())
            }
        }
    }

    fn decode_body(body: Bytes) -> Result<Self> {
        let (&tag, rest) = body.split_first().ok_or(Error::InvalidData)?;
        match tag {
            EXIT_TAG_BOUND => {
                let (local_addr, tail) = parse_sock_addr(rest)?;
                if !tail.is_empty() || local_addr.port() == 0 {
                    return Err(Error::InvalidData);
                }
                Ok(Self::Bound { local_addr })
            }
            EXIT_TAG_DATAGRAM => {
                let (peer, tail) = parse_sock_addr(rest)?;
                if tail.len() > MAX_UDP_DATAGRAM_BYTES {
                    return Err(Error::InvalidData);
                }
                let payload_offset = body.len() - tail.len();
                Ok(Self::Datagram {
                    peer,
                    payload: body.slice(payload_offset..),
                })
            }
            EXIT_TAG_PEER_ALLOWED => {
                let (ip, tail) = parse_ip(rest)?;
                if !tail.is_empty() {
                    return Err(Error::InvalidData);
                }
                Ok(Self::PeerAllowed { ip })
            }
            _ => Err(Error::InvalidData),
        }
    }
}

/// Bounded streaming decoder for length-delimited `onion::udp` frames.
#[derive(Debug, Default)]
pub(crate) struct OnionUdpFrameDecoder {
    buf: BytesMut,
}

impl OnionUdpFrameDecoder {
    /// Append `chunk` to the internal reassembly buffer, rejecting oversized unconsumed buffers
    /// or illegal length headers immediately.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<()> {
        if self.buf.len().saturating_add(chunk.len()) > MAX_DECODER_BUFFER_BYTES {
            return Err(Error::InvalidData);
        }
        self.buf.extend_from_slice(chunk);
        if self.buf.len() >= FRAME_LEN_HEADER_BYTES {
            let len = self.peek_body_len()?;
            if len == 0 || len > MAX_UDP_FRAME_BODY_BYTES {
                return Err(Error::InvalidData);
            }
        }
        Ok(())
    }

    fn peek_body_len(&self) -> Result<usize> {
        let header: [u8; 4] = self
            .buf
            .get(..FRAME_LEN_HEADER_BYTES)
            .ok_or(Error::InvalidData)?
            .try_into()
            .map_err(|_| Error::InvalidData)?;
        usize::try_from(u32::from_be_bytes(header)).map_err(|_| Error::InvalidData)
    }

    fn pop_raw_body(&mut self) -> Result<Option<Bytes>> {
        if self.buf.len() < FRAME_LEN_HEADER_BYTES {
            return Ok(None);
        }
        let body_len = self.peek_body_len()?;
        if body_len == 0 || body_len > MAX_UDP_FRAME_BODY_BYTES {
            return Err(Error::InvalidData);
        }
        if self.buf.len() < FRAME_LEN_HEADER_BYTES + body_len {
            return Ok(None);
        }
        self.buf.advance(FRAME_LEN_HEADER_BYTES);
        Ok(Some(self.buf.split_to(body_len).freeze()))
    }

    /// Decode the next complete [`OnionUdpClientFrame`], if available.
    pub(crate) fn next_client_frame(&mut self) -> Result<Option<OnionUdpClientFrame>> {
        self.pop_raw_body()?
            .map(OnionUdpClientFrame::decode_body)
            .transpose()
    }

    /// Decode the next complete [`OnionUdpExitFrame`], if available.
    pub(crate) fn next_exit_frame(&mut self) -> Result<Option<OnionUdpExitFrame>> {
        self.pop_raw_body()?
            .map(OnionUdpExitFrame::decode_body)
            .transpose()
    }
}
