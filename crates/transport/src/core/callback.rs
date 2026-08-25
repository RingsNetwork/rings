//! The main entity of this module is the [TransportCallback] trait, which defines
//! a series of methods that receive connection events.
//!
//! The `new_connection` method of
//! [TransportInterface](super::transport::TransportInterface) trait will
//! accept boxed [TransportCallback] trait object.

use async_trait::async_trait;

use crate::core::transport::WebrtcConnectionState;

type CallbackError = Box<dyn std::error::Error>;

/// Coarse inbound traffic class used by core callback scheduling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboundFrameClass {
    /// DHT and connection-liveness traffic needed to make protocol progress.
    Control,
    /// Storage protocol traffic.
    Storage,
    /// End-to-end protocol traffic.
    EndToEnd,
    /// Application traffic.
    Application,
    /// Chunk envelopes awaiting reassembly.
    Reassembly,
    /// Malformed, unknown, or otherwise unclassified data traffic.
    Data,
}

/// One inbound payload whose transport envelope and raw-frame capacity were admitted.
///
/// Only the transport admission layer can construct this value. Public transport
/// adapters can therefore dispatch messages only after synchronous admission.
pub struct AdmittedInboundMessage<'a> {
    cid: &'a str,
    payload: &'a [u8],
}

impl<'a> AdmittedInboundMessage<'a> {
    pub(crate) const fn new(cid: &'a str, payload: &'a [u8]) -> Self {
        Self { cid, payload }
    }

    /// Return the connection identifier associated with this frame.
    pub const fn cid(&self) -> &str {
        self.cid
    }

    /// Return the decoded custom transport payload.
    pub const fn payload(&self) -> &[u8] {
        self.payload
    }

    /// Consume this admission and return its connection identifier and payload.
    pub const fn into_parts(self) -> (&'a str, &'a [u8]) {
        (self.cid, self.payload)
    }
}

/// Any object that implements this trait can be used as a callback for the connection.
#[cfg_attr(all(feature = "web-sys-webrtc", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(
    not(all(feature = "web-sys-webrtc", target_family = "wasm")),
    async_trait
)]
pub trait TransportCallback {
    /// Notify the data channel is open.
    async fn on_data_channel_open(&self, _cid: &str) -> Result<(), CallbackError> {
        Ok(())
    }

    /// Notify the data channel is closed. This is a reliable, prompt signal that
    /// the peer has gone (e.g. it closed the connection): unlike the ICE
    /// `Disconnected` state it does not fire on transient blips, so the swarm
    /// can use it to tear the connection down without waiting for `Failed`.
    async fn on_data_channel_close(&self, _cid: &str) -> Result<(), CallbackError> {
        Ok(())
    }

    /// Handle a message together with its non-forgeable raw-frame admission.
    async fn on_admitted_message(
        &self,
        _message: AdmittedInboundMessage<'_>,
    ) -> Result<(), CallbackError> {
        Ok(())
    }

    /// Record a frame rejected as malformed or oversized before core dispatch.
    ///
    /// Local capacity pressure must not call this method because it is not
    /// evidence of remote peer failure.
    async fn on_invalid_inbound_frame(&self, _cid: &str) -> Result<(), CallbackError> {
        Ok(())
    }

    /// This method is invoked when the state of connection has changed.
    async fn on_peer_connection_state_change(
        &self,
        _cid: &str,
        _state: WebrtcConnectionState,
    ) -> Result<(), CallbackError> {
        Ok(())
    }
}

/// The `new_connection` method of
/// [TransportInterface](super::transport::TransportInterface) trait will
/// accept boxed [TransportCallback] trait object.
#[cfg(not(all(feature = "web-sys-webrtc", target_family = "wasm")))]
pub type BoxedTransportCallback = Box<dyn TransportCallback + Send + Sync>;

/// The `new_connection` method of
/// [TransportInterface](super::transport::TransportInterface) trait will
/// accept boxed [TransportCallback] trait object.
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
pub type BoxedTransportCallback = Box<dyn TransportCallback>;
