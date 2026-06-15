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
//! future — "buffered" is simply the future still being `Pending`. The future
//! drives its own wake-ups, so callers can just spawn it and forget it.

use std::future::Future;
use std::pin::Pin;

use crate::error::Result;

/// A future resolving to the eventual fate of a sent message: `Ok(())` once the
/// bytes are flushed to the wire, `Err(..)` if the channel closed while they
/// were still buffered.
///
/// It is `Send` on native targets (so it can be spawned on a multi-threaded
/// runtime) and `!Send` on wasm, matching the rest of the transport.
#[cfg(feature = "web-sys-webrtc")]
pub type DeliveryFuture = Pin<Box<dyn Future<Output = Result<()>>>>;

/// A future resolving to the eventual fate of a sent message.
#[cfg(not(feature = "web-sys-webrtc"))]
pub type DeliveryFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
