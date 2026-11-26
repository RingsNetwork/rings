//! The send half of credit flow control: the one place where a backend's sends meet credit.
//!
//! A backend provides only its raw send of a wire frame ([`FrameSink`]); every frame it sends
//! passes through here or through the credit pump, so no backend holds any of the credit law:
//!
//! ```text
//!   send_message_with_permit(payload) ─▶ send_custom ─open─▶ credit ─▶ send_frame(Custom)
//!   credit pump ─next_signal─────────────▶ send_signal ─open─────────▶ send_frame(Signal)
//! ```
//!
//! The public send API takes a custom payload, so a credit frame leaves this end only from its
//! own pump (see [`TransportMessage`]).

use async_trait::async_trait;
use bytes::Bytes;

use super::InnerTransportCallback;
use crate::core::pool::ChannelLane;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportMessage;
use crate::delivery::DeliveryFuture;
use crate::error::Error;
use crate::error::Result;

/// A connection backend's raw send: the half of a connection beneath credit.
///
/// Pre of [`send_frame`](Self::send_frame): the frame's credit, if it needs one, is already
/// held by the caller. The backend meets the contract of
/// [`ConnectionInterface::send_message_with_permit`] for `permit` and nothing more.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub(crate) trait FrameSink: ConnectionInterface<Error = Error> {
    /// Encode `frame` and hand it to the data channel `lane` is pinned to, under `permit`.
    async fn send_frame(
        &self,
        frame: TransportMessage,
        lane: ChannelLane,
        permit: SendPermit,
    ) -> Result<DeliveryFuture>;
}

/// Return the exact `postcard` wire length of `TransportMessage::Custom(payload)` for a payload
/// of `payload_len` bytes (`1` discriminant byte + LEB128 varint length + `payload_len`).
const fn custom_frame_wire_len(payload_len: usize) -> Option<usize> {
    let mut n = payload_len;
    let mut varint_bytes = 1_usize;
    while n >= 0x80 {
        n >>= 7;
        varint_bytes += 1;
    }
    let Some(header_bytes) = 1_usize.checked_add(varint_bytes) else {
        return None;
    };
    header_bytes.checked_add(payload_len)
}

impl InnerTransportCallback {
    /// Send the custom `payload` through `sink` on `lane`, once the data channels are open,
    /// verifying `wire.len() <= sink.max_message_size()` before reserving one credit of `lane`
    /// (the one `permit` carries, or else one reserved now).
    ///
    /// The credit is held until the send is decided and then settled by its outcome (see
    /// [`CreditReservation`](super::link_credit::CreditReservation)).
    pub(crate) async fn send_custom<S>(
        &self,
        sink: &S,
        payload: Bytes,
        lane: ChannelLane,
        mut permit: SendPermit,
    ) -> Result<DeliveryFuture>
    where
        S: FrameSink + ?Sized,
    {
        sink.wait_for_channel_open().await?;
        let max = sink.max_message_size();
        let size = custom_frame_wire_len(payload.len()).ok_or(Error::SendByteCountOverflow)?;
        if size > max {
            return Err(Error::FrameTooLarge { size, max });
        }
        let _credit = self.credit_for_custom_send(lane, &mut permit).await?;
        sink.send_frame(TransportMessage::Custom(payload), lane, permit)
            .await
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::custom_frame_wire_len;
    use crate::core::transport::TransportMessage;

    #[test]
    fn test_custom_frame_wire_len_matches_postcard_encoding() {
        for payload_len in [0_usize, 1, 127, 128, 255, 16_383, 16_384, 65_536] {
            let payload = Bytes::from(vec![0xAB; payload_len]);
            let encoded = rings_codec::serialize(&TransportMessage::Custom(payload)).unwrap();
            assert_eq!(
                custom_frame_wire_len(payload_len),
                Some(encoded.len()),
                "mismatch at payload_len={payload_len}"
            );
        }
    }
}
