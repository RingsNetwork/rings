//! Shared RFC 5389 STUN and RFC 5766 TURN-over-TCP (`ChannelData`) wire framing helpers.

#[cfg(all(feature = "turn-tcp-io", not(target_family = "wasm")))]
use std::time::Duration;

#[cfg(all(feature = "turn-tcp-io", not(target_family = "wasm")))]
use tokio::io::AsyncRead;
#[cfg(all(feature = "turn-tcp-io", not(target_family = "wasm")))]
use tokio::io::AsyncReadExt;

/// Fixed byte length of an RFC 5389 STUN message header.
pub const STUN_HEADER_BYTES: usize = 20;

/// Fixed RFC 5389 STUN magic cookie (`0x2112A442`).
pub const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];

/// Fixed byte length of an RFC 5766 `ChannelData` header.
pub const CHANNEL_DATA_HEADER_BYTES: usize = 4;

/// Lowest RFC 5766 `ChannelData` channel number (`0x4000`).
pub const CHANNEL_NUMBER_MIN: u16 = 0x4000;

/// Highest RFC 5766 `ChannelData` channel number (`0x7FFF`).
pub const CHANNEL_NUMBER_MAX: u16 = 0x7FFF;

/// Allocation-free check whether `datagram` is a well-formed RFC 5389 STUN message or RFC 5766
/// `ChannelData` datagram that can be framed over a TCP/TLS TURN stream.
pub fn is_valid_turn_tcp_datagram(datagram: &[u8]) -> bool {
    let &[b0, b1, b2, b3, ..] = datagram else {
        return false;
    };
    let first_u16 = u16::from_be_bytes([b0, b1]);
    let length_u16 = usize::from(u16::from_be_bytes([b2, b3]));

    if (b0 & 0b1100_0000) == 0 {
        if (length_u16 & 0b11) != 0 || datagram.get(4..8) != Some(&STUN_MAGIC_COOKIE) {
            return false;
        }
        STUN_HEADER_BYTES
            .checked_add(length_u16)
            .is_some_and(|total| datagram.len() >= total)
    } else if (CHANNEL_NUMBER_MIN..=CHANNEL_NUMBER_MAX).contains(&first_u16) {
        CHANNEL_DATA_HEADER_BYTES
            .checked_add(length_u16)
            .is_some_and(|total| datagram.len() >= total)
    } else {
        false
    }
}

/// Encode an outbound STUN message or TURN `ChannelData` datagram for transmission over a
/// stream-oriented TCP/TLS TURN connection (RFC 5766 §7.3 & §11.5).
pub fn encode_turn_tcp_frame(datagram: &[u8]) -> Option<Vec<u8>> {
    if !is_valid_turn_tcp_datagram(datagram) {
        return None;
    }
    let &[b0, _b1, b2, b3, ..] = datagram else {
        return None;
    };
    let length_u16 = usize::from(u16::from_be_bytes([b2, b3]));

    if (b0 & 0b1100_0000) == 0 {
        let total = STUN_HEADER_BYTES.checked_add(length_u16)?;
        Some(datagram.get(..total)?.to_vec())
    } else {
        let unpadded_total = CHANNEL_DATA_HEADER_BYTES.checked_add(length_u16)?;
        let slice = datagram.get(..unpadded_total)?;
        let padded_data_len = (length_u16 + 3) & !3;
        let padded_total = CHANNEL_DATA_HEADER_BYTES.checked_add(padded_data_len)?;
        let mut out = Vec::with_capacity(padded_total);
        out.extend_from_slice(slice);
        out.resize(padded_total, 0u8);
        Some(out)
    }
}

/// Return whether `datagram` is an RFC 5389 STUN Success Response (`0b10`) or Error Response (`0b11`).
pub fn is_stun_control_response(datagram: &[u8]) -> bool {
    let Some(&b0) = datagram.first() else {
        return false;
    };
    if datagram.get(4..8) != Some(&STUN_MAGIC_COOKIE) {
        return false;
    }
    // RFC 5389 Figure 3: STUN messages have top 2 bits 00, and STUN class bit C1 (bit 0 of byte 0)
    // is 1 for both Success Response (class 0b10) and Error Response (class 0b11), and 0 for
    // Request (0b00) and Indication (0b01, such as Data Indication 0x0017).
    (b0 & 0b1100_0001) == 0b0000_0001
}

/// Read the remainder of a single RFC 5766 STUN message or `ChannelData` frame given its already
/// read 4-byte TCP `prefix`, validating alignment, cookie, and `max_frame_bytes`.
#[cfg(all(feature = "turn-tcp-io", not(target_family = "wasm")))]
pub async fn read_turn_tcp_frame_body<R>(
    reader: &mut R,
    prefix: [u8; 4],
    max_frame_bytes: usize,
    body_timeout: Duration,
) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let [b0, b1, b2, b3] = prefix;
    let first_u16 = u16::from_be_bytes([b0, b1]);
    let length_u16 = usize::from(u16::from_be_bytes([b2, b3]));

    if (b0 & 0b1100_0000) == 0 {
        if (length_u16 & 0b11) != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unaligned STUN message length over TCP: {length_u16}"),
            ));
        }
        let total = STUN_HEADER_BYTES.checked_add(length_u16).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "STUN frame size overflow")
        })?;
        if total > max_frame_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("STUN frame size {total} exceeds maximum {max_frame_bytes}"),
            ));
        }
        let mut hdr_rest = [0u8; 16];
        tokio::time::timeout(body_timeout, reader.read_exact(&mut hdr_rest))
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out reading STUN header over TCP",
                )
            })??;
        if hdr_rest.get(..4) != Some(&STUN_MAGIC_COOKIE) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid STUN magic cookie on TCP stream",
            ));
        }
        let mut frame = vec![0u8; total];
        if let Some(head) = frame.get_mut(..4) {
            head.copy_from_slice(&prefix);
        }
        if let Some(rest) = frame.get_mut(4..STUN_HEADER_BYTES) {
            rest.copy_from_slice(&hdr_rest);
        }
        if let Some(body) = frame.get_mut(STUN_HEADER_BYTES..) {
            if !body.is_empty() {
                tokio::time::timeout(body_timeout, reader.read_exact(body))
                    .await
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "timed out reading STUN body over TCP",
                        )
                    })??;
            }
        }
        Ok(frame)
    } else if (CHANNEL_NUMBER_MIN..=CHANNEL_NUMBER_MAX).contains(&first_u16) {
        let padded_body_bytes = (length_u16 + 3) & !3;
        let total = CHANNEL_DATA_HEADER_BYTES
            .checked_add(padded_body_bytes)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "ChannelData frame size overflow",
                )
            })?;
        if total > max_frame_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("ChannelData frame size {total} exceeds maximum {max_frame_bytes}"),
            ));
        }
        let mut frame = vec![0u8; total];
        if let Some(head) = frame.get_mut(..4) {
            head.copy_from_slice(&prefix);
        }
        if let Some(body) = frame.get_mut(4..) {
            if !body.is_empty() {
                tokio::time::timeout(body_timeout, reader.read_exact(body))
                    .await
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "timed out reading ChannelData body over TCP",
                        )
                    })??;
            }
        }
        Ok(frame)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid TURN TCP frame header: 0x{first_u16:04x}"),
        ))
    }
}

/// Read a single RFC 5766 STUN message or `ChannelData` frame from a TCP/TLS stream.
#[cfg(all(feature = "turn-tcp-io", not(target_family = "wasm")))]
pub async fn read_turn_tcp_frame<R>(
    reader: &mut R,
    max_frame_bytes: usize,
    body_timeout: Duration,
) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0u8; 4];
    reader.read_exact(&mut prefix).await?;
    read_turn_tcp_frame_body(reader, prefix, max_frame_bytes, body_timeout).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_and_validate_rejects_malformed_and_pads_channel_data() {
        assert!(!is_valid_turn_tcp_datagram(&[]));
        assert!(encode_turn_tcp_frame(&[0x00, 0x01, 0x00, 0x00]).is_none());

        // ChannelData with 5-byte payload pads to 4 + 8 bytes over TCP.
        let cd = vec![0x40, 0x01, 0x00, 0x05, 1, 2, 3, 4, 5];
        assert!(is_valid_turn_tcp_datagram(&cd));
        let framed = encode_turn_tcp_frame(&cd).expect("valid ChannelData");
        assert_eq!(framed.len(), 12);
        assert_eq!(&framed[..9], cd.as_slice());
        assert_eq!(&framed[9..], &[0, 0, 0]);
        assert!(!is_stun_control_response(&framed));

        // Unaligned STUN length is rejected.
        let mut unaligned_stun = vec![0x00, 0x01, 0x00, 0x03];
        unaligned_stun.extend_from_slice(&STUN_MAGIC_COOKIE);
        unaligned_stun.extend_from_slice(&[0u8; 16]);
        assert!(!is_valid_turn_tcp_datagram(&unaligned_stun));
        assert!(encode_turn_tcp_frame(&unaligned_stun).is_none());

        // Valid STUN Binding Success Response (0x0101) vs Binding Request (0x0001).
        let mut stun_resp = vec![0x01, 0x01, 0x00, 0x00];
        stun_resp.extend_from_slice(&STUN_MAGIC_COOKIE);
        stun_resp.extend_from_slice(&[7u8; 12]);
        assert!(is_stun_control_response(&stun_resp));
        assert_eq!(
            encode_turn_tcp_frame(&stun_resp).as_deref(),
            Some(stun_resp.as_slice())
        );

        let mut stun_req = vec![0x00, 0x01, 0x00, 0x00];
        stun_req.extend_from_slice(&STUN_MAGIC_COOKIE);
        stun_req.extend_from_slice(&[7u8; 12]);
        assert!(!is_stun_control_response(&stun_req));
    }

    #[cfg(all(feature = "turn-tcp-io", not(target_family = "wasm")))]
    #[tokio::test]
    async fn read_turn_tcp_frame_enforces_max_frame_bytes_and_decodes_roundtrip() {
        let mut stun_req = vec![0x00, 0x01, 0x00, 0x04];
        stun_req.extend_from_slice(&STUN_MAGIC_COOKIE);
        stun_req.extend_from_slice(&[9u8; 12]);
        stun_req.extend_from_slice(&[1, 2, 3, 4]);
        let framed = encode_turn_tcp_frame(&stun_req).unwrap();

        let mut cursor = std::io::Cursor::new(framed.clone());
        let decoded = read_turn_tcp_frame(&mut cursor, 64, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(decoded, framed);

        let mut small_cap = std::io::Cursor::new(framed);
        let err = read_turn_tcp_frame(&mut small_cap, 20, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
