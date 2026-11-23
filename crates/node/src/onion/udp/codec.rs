//! Length-prefixed datagram and pinhole control frames carried over an `onion::udp` session.

use std::net::IpAddr;
use std::net::SocketAddr;

use bytes::Buf;
use bytes::BufMut;
use bytes::BytesMut;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;

/// Length header width in bytes (`u32` big-endian) preceding every `onion::udp` frame.
pub const ONION_UDP_FRAME_LEN_BYTES: usize = 4;

/// Maximum raw UDP payload carried by a single [`OnionUdpClientFrame::SendDatagram`] or
/// [`OnionUdpServerFrame::RecvDatagram`] (`65,535 - 20 (IPv4) - 8 (UDP) = 65,507` bytes).
pub const MAX_ONION_UDP_PAYLOAD_BYTES: usize = 65_507;

/// Maximum serialized byte length of a single `onion::udp` frame body (excluding the 4-byte length
/// prefix). Bounds per-session frame reassembly memory on both client and exit.
pub const MAX_ONION_UDP_FRAME_BYTES: usize = 65_568;

/// Client-to-exit frame carried inside `onion::udp` forward session data.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum OnionUdpClientFrame {
    /// Open or refresh an inbound pinhole permitting UDP datagrams from `peer_ip`.
    AllowPeerIp {
        /// Remote peer IP address permitted to send datagrams to the bound exit socket.
        peer_ip: IpAddr,
        /// Requested pinhole lifetime in milliseconds (clamped by the exit).
        ttl_ms: u64,
    },
    /// Send one UDP datagram from the exit's bound socket to `peer`.
    SendDatagram {
        /// Destination UDP socket address.
        peer: SocketAddr,
        /// Raw UDP payload bytes.
        payload: Vec<u8>,
    },
}

/// Exit-to-client frame carried inside `onion::udp` return session data.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum OnionUdpServerFrame {
    /// Initial frame emitted immediately after session open reporting the exit's bound socket
    /// endpoint.
    Bound {
        /// Externally reachable UDP socket address bound by the exit for this session.
        local_addr: SocketAddr,
    },
    /// Inbound UDP datagram received on the exit's bound socket from a permitted peer IP.
    RecvDatagram {
        /// Source UDP socket address observed by the exit's OS socket.
        peer: SocketAddr,
        /// Raw UDP payload bytes.
        payload: Vec<u8>,
    },
}

/// Encode one [`OnionUdpClientFrame`] with its 4-byte big-endian length prefix.
pub fn encode_client_frame(frame: &OnionUdpClientFrame) -> Result<Vec<u8>> {
    if let OnionUdpClientFrame::SendDatagram { payload, .. } = frame {
        if payload.len() > MAX_ONION_UDP_PAYLOAD_BYTES {
            return Err(Error::InvalidData);
        }
    }
    encode_length_prefixed(frame)
}

/// Decode the next complete [`OnionUdpClientFrame`] from `buf`, leaving any trailing partial frame
/// untouched. Returns `Err(Error::InvalidData)` on oversized frames or malformed payloads.
pub fn decode_next_client_frame(buf: &mut BytesMut) -> Result<Option<OnionUdpClientFrame>> {
    let Some(frame) = decode_next_length_prefixed::<OnionUdpClientFrame>(buf)? else {
        return Ok(None);
    };
    if let OnionUdpClientFrame::SendDatagram { payload, .. } = &frame {
        if payload.len() > MAX_ONION_UDP_PAYLOAD_BYTES {
            return Err(Error::InvalidData);
        }
    }
    Ok(Some(frame))
}

/// Encode one [`OnionUdpServerFrame`] with its 4-byte big-endian length prefix.
pub fn encode_server_frame(frame: &OnionUdpServerFrame) -> Result<Vec<u8>> {
    if let OnionUdpServerFrame::RecvDatagram { payload, .. } = frame {
        if payload.len() > MAX_ONION_UDP_PAYLOAD_BYTES {
            return Err(Error::InvalidData);
        }
    }
    encode_length_prefixed(frame)
}

/// Decode the next complete [`OnionUdpServerFrame`] from `buf`, leaving any trailing partial frame
/// untouched. Returns `Err(Error::InvalidData)` on oversized frames or malformed payloads.
pub fn decode_next_server_frame(buf: &mut BytesMut) -> Result<Option<OnionUdpServerFrame>> {
    let Some(frame) = decode_next_length_prefixed::<OnionUdpServerFrame>(buf)? else {
        return Ok(None);
    };
    if let OnionUdpServerFrame::RecvDatagram { payload, .. } = &frame {
        if payload.len() > MAX_ONION_UDP_PAYLOAD_BYTES {
            return Err(Error::InvalidData);
        }
    }
    Ok(Some(frame))
}

fn encode_length_prefixed<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let body = rings_codec::serialize(value).map_err(|_| Error::InvalidData)?;
    if body.is_empty() || body.len() > MAX_ONION_UDP_FRAME_BYTES {
        return Err(Error::InvalidData);
    }
    let len = u32::try_from(body.len()).map_err(|_| Error::InvalidData)?;
    let mut framed = Vec::with_capacity(ONION_UDP_FRAME_LEN_BYTES + body.len());
    framed.put_u32(len);
    framed.extend_from_slice(&body);
    Ok(framed)
}

fn decode_next_length_prefixed<T: for<'de> Deserialize<'de>>(
    buf: &mut BytesMut,
) -> Result<Option<T>> {
    let &[b0, b1, b2, b3, ..] = buf.as_ref() else {
        return Ok(None);
    };
    let body_len_u32 = u32::from_be_bytes([b0, b1, b2, b3]);
    let body_len = usize::try_from(body_len_u32).map_err(|_| Error::InvalidData)?;
    if body_len == 0 || body_len > MAX_ONION_UDP_FRAME_BYTES {
        return Err(Error::InvalidData);
    }
    let total_len = ONION_UDP_FRAME_LEN_BYTES
        .checked_add(body_len)
        .ok_or(Error::InvalidData)?;
    if buf.len() < total_len {
        return Ok(None);
    }
    buf.advance(ONION_UDP_FRAME_LEN_BYTES);
    let body = buf.split_to(body_len);
    let decoded = rings_codec::deserialize::<T>(&body).map_err(|_| Error::InvalidData)?;
    Ok(Some(decoded))
}
