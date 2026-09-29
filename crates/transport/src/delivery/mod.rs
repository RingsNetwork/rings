//! Delivery tracking for sent data channel messages.
//!
//! A normal send only confirms the bytes were accepted into the local send
//! buffer, not that they actually left for the wire. That distinction matters
//! for "optimistic send": when a connection is transiently `Disconnected` the
//! data channel stays open and `send` keeps buffering, so the bytes are
//! silently lost if the connection later fails.
//!
//! To surface this without threading a status type through every layer, a send
//! returns a [DeliveryFuture]: a self-contained future, constructed at the
//! moment of send, that resolves to `Ok(())` once the bytes have been flushed
//! to the wire or `Err(..)` if the data channel closed first. It compresses the
//! three underlying states (buffered / flushed / lost) into a two-outcome
//! future — "buffered" is simply the future still being `Pending`.
//!
//! The future is event-driven. Each data channel owns one
//! delivery tracker (`tracker::DeliveryTracker`) whose pure `registry` multiplexes the
//! channel's single `bufferedAmountLowThreshold` over every pending send; the
//! channel's `bufferedamountlow`, `close` and `error` events are the only
//! wake-ups, and no timer takes part in a verdict. Callers can still spawn the
//! future and forget it: dropping it removes its slot, and the next settle
//! round re-arms the threshold for the sends that remain.
//!
//! "Flushed" means the bytes left the channel's `bufferedAmount`. On native
//! webrtc-rs that happens only when the peer's SCTP SACK acknowledges them, so
//! there the verdict is "acknowledged by the peer", which is stronger than
//! "handed to the wire".

use std::future::Future;
use std::pin::Pin;

use crate::error::Result;

#[cfg(any(
    all(feature = "native-webrtc", not(target_family = "wasm")),
    all(feature = "web-sys-webrtc", target_family = "wasm")
))]
mod registry;
#[cfg(all(test, feature = "native-webrtc", not(target_family = "wasm")))]
mod test_tracker;
#[cfg(any(
    all(feature = "native-webrtc", not(target_family = "wasm")),
    all(feature = "web-sys-webrtc", target_family = "wasm")
))]
pub(crate) mod tracker;

/// Flush predicate `φ(E, b, e) ≜ E ⊖ b ≥ e`: the bytes ending at `end_offset`
/// have left the local buffer once `enqueued − buffered` reaches it.
#[cfg(any(
    all(feature = "native-webrtc", not(target_family = "wasm")),
    all(feature = "web-sys-webrtc", target_family = "wasm")
))]
pub(crate) const fn delivery_flushed(enqueued: u64, buffered: u64, end_offset: u64) -> bool {
    enqueued.saturating_sub(buffered) >= end_offset
}

/// The error a delivery future reports when its channel closes before the flush.
#[cfg(any(
    all(feature = "native-webrtc", not(target_family = "wasm")),
    all(feature = "web-sys-webrtc", target_family = "wasm")
))]
pub(crate) fn closed_before_flush() -> crate::error::Error {
    crate::error::Error::MessageNotDelivered(
        "data channel closed before the message was flushed".to_string(),
    )
}

/// A future resolving to the eventual fate of a sent message: `Ok(())` once the
/// bytes are flushed to the wire, `Err(..)` if the channel closed while they
/// were still buffered.
///
/// It is `Send` on native targets (so it can be spawned on a multi-threaded
/// runtime) and `!Send` on wasm, matching the rest of the transport.
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
pub type DeliveryFuture = Pin<Box<dyn Future<Output = Result<()>>>>;

/// A future resolving to the eventual fate of a sent message.
#[cfg(not(all(feature = "web-sys-webrtc", target_family = "wasm")))]
pub type DeliveryFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

#[cfg(all(
    test,
    any(
        all(feature = "native-webrtc", not(target_family = "wasm")),
        all(feature = "web-sys-webrtc", target_family = "wasm")
    )
))]
mod tests {
    use super::delivery_flushed;

    #[test]
    fn test_flush_predicate_is_monotonic_and_saturates_under_inconsistent_observation() {
        assert!(!delivery_flushed(9, 4, 6));
        assert!(delivery_flushed(10, 4, 6));
        assert!(delivery_flushed(11, 4, 6));
        assert!(!delivery_flushed(4, 5, 1));
    }
}
