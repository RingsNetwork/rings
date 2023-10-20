//! The main entity of this module is the [Callback] trait, which defines
//! a series of methods that receive connection events.
//!
//! The `new_connection` method of
//! [ConnectionCreation](super::transport::ConnectionCreation) trait will
//! accept boxed [Callback] trait object.

use async_trait::async_trait;

use crate::core::transport::WebrtcConnectionState;

type CallbackError = Box<dyn std::error::Error>;

/// Any object that implements this trait can be used as a callback for the connection.
#[cfg_attr(feature = "web-sys-webrtc", async_trait(?Send))]
#[cfg_attr(not(feature = "web-sys-webrtc"), async_trait)]
pub trait TransportCallback {
    /// This method is invoked on a binary message arrival over the data channel of webrtc.
    async fn on_message(&self, _cid: &str, _msg: &[u8]) -> Result<(), CallbackError> {
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
/// [ConnectionCreation](super::transport::ConnectionCreation) trait will
/// accept boxed [Callback] trait object.
#[cfg(not(feature = "web-sys-webrtc"))]
pub type BoxedTransportCallback = Box<dyn TransportCallback + Send + Sync>;

/// The `new_connection` method of
/// [ConnectionCreation](super::transport::ConnectionCreation) trait will
/// accept boxed [Callback] trait object.
#[cfg(feature = "web-sys-webrtc")]
pub type BoxedTransportCallback = Box<dyn TransportCallback>;
