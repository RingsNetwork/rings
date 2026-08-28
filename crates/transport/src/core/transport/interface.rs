use std::net::IpAddr;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use super::ConnectionStateSnapshot;
use super::SendPermit;
use super::WebrtcConnectionState;
use crate::callback::InboundFrameCapacity;
use crate::connection_ref::ConnectionRef;
use crate::core::callback::BoxedTransportCallback;
use crate::core::sdp::parse_sdp_max_message_size;
use crate::delivery::DeliveryFuture;

macro_rules! define_transport_messages {
    ($( $(#[$docs:meta])* $variant:ident ),+ $(,)?) => {
        /// Wrapper for the data that is sent over the data channel.
        #[derive(Deserialize, Serialize, Debug, Clone)]
        pub enum TransportMessage {
            $(
                $(#[$docs])*
                $variant(Bytes),
            )+
        }

        #[derive(Deserialize)]
        pub(crate) enum BorrowedTransportMessage<'a> {
            $($variant(#[serde(borrow)] &'a [u8]),)+
        }
    };
}

define_transport_messages!(
    /// A custom message sent by an external invoker and handled by the
    /// `on_admitted_message` callback. Since 0.18 this stores [`Bytes`]
    /// instead of `Vec<u8>` without changing its wire encoding.
    Custom
);

/// Interop ceiling for a single data-channel message, in bytes - RFC 8841's default
/// `max-message-size` (65536), the value a spec-compliant peer accepts when it advertises nothing
/// else. We treat it as a hard send ceiling: a sender never exceeds it regardless of what the
/// remote advertises, and a per-channel
/// [`max_message_size`](ConnectionInterface::max_message_size) may resolve to *less* (a constrained
/// peer) but never more. NOTE: this is the protocol default, not an independently verified property
/// of every backend's SCTP stack - a peer advertising a *larger* limit is still clamped to this.
pub const MAX_DATA_CHANNEL_MESSAGE_SIZE: usize = 65536;

/// Decode the internal `0 = not negotiated` sentinel used by connection backends.
#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    feature = "web-sys-webrtc"
))]
pub(crate) const fn stored_max_message_size(stored: usize) -> usize {
    if stored == 0 {
        MAX_DATA_CHANNEL_MESSAGE_SIZE
    } else {
        stored
    }
}

/// The effective per-message send limit for a peer whose SDP is `remote_sdp`. The negotiated value
/// is parsed from the SDP by [`crate::core::sdp`]; this function is the *policy* layered on top.
/// Per RFC 8841 an absent attribute defaults to 65536 and a value of `0` means "no limit" (we still
/// bound it by our own send cap); any explicit value is honoured but capped at
/// [`MAX_DATA_CHANNEL_MESSAGE_SIZE`] for interop. Always returns a positive value.
pub fn effective_max_message_size(remote_sdp: &str) -> usize {
    match parse_sdp_max_message_size(remote_sdp) {
        None | Some(0) => MAX_DATA_CHANNEL_MESSAGE_SIZE,
        Some(n) => (n as usize).min(MAX_DATA_CHANNEL_MESSAGE_SIZE),
    }
}

/// The [ConnectionInterface] trait defines how to
/// make webrtc ice handshake with a remote peer and then send data channel message to it.
#[cfg_attr(all(feature = "web-sys-webrtc", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(
    not(all(feature = "web-sys-webrtc", target_family = "wasm")),
    async_trait
)]
pub trait ConnectionInterface {
    /// Sdp is used to expose local and remote session descriptions when handshaking.
    type Sdp: Serialize + DeserializeOwned;
    /// The error type that is returned by connection.
    type Error: std::error::Error;

    /// Send a [TransportMessage] to the remote peer.
    ///
    /// The returned `Result` reflects whether the bytes were accepted into the
    /// local send buffer. The [DeliveryFuture] it yields resolves later to the
    /// message's actual fate: `Ok(())` once flushed to the wire, or `Err(..)`
    /// if the channel closed while the bytes were still buffered. Callers that
    /// don't care can drop it; callers that do can spawn it (see
    /// [crate::delivery]).
    async fn send_message(&self, msg: TransportMessage) -> Result<DeliveryFuture, Self::Error> {
        self.send_message_with_permit(msg, SendPermit::always())
            .await
    }

    /// Send only if `permit` holds at the final cancellable backend boundary.
    ///
    /// Before the first write, spawn, or `.await` that can continue after this
    /// returned future is dropped, implementations must synchronously call
    /// [`SendPermit::try_mark_irrevocable`] and proceed only when it returns a
    /// proof token. This requirement also applies to synchronous send primitives
    /// so higher layers can atomically arbitrate deadlines at the same boundary.
    /// After the backend accepts the bytes, implementations must consume that
    /// proof with
    /// [`IrrevocableSendPermit::mark_accepted`](crate::core::transport::IrrevocableSendPermit::mark_accepted)
    /// before returning
    /// success. If work fails or is abandoned after claiming the proof but before
    /// acceptance, the implementation must retire and close that connection
    /// generation before returning.
    async fn send_message_with_permit(
        &self,
        msg: TransportMessage,
        permit: SendPermit,
    ) -> Result<DeliveryFuture, Self::Error>;

    /// Get current webrtc connection state.
    fn webrtc_connection_state(&self) -> WebrtcConnectionState;

    /// Return one coherent WebRTC/data-channel product-state observation.
    fn connection_state_snapshot(&self) -> ConnectionStateSnapshot;

    /// Return whether every data channel used by this connection is currently open.
    ///
    /// This is one component of routability, not the complete predicate. ICE
    /// may still report `Connecting` when SCTP has opened, while
    /// `Disconnected + Open` must not be treated as ready. Callers must classify
    /// the WebRTC/data-channel product state according to their protocol model.
    fn data_channel_is_open(&self) -> Result<bool, Self::Error>;

    /// The maximum size, in bytes, of one message this connection can send - the channel's
    /// negotiated SCTP / data-channel `max_message_size`, capped at
    /// [`MAX_DATA_CHANNEL_MESSAGE_SIZE`] for cross-peer interop. A caller must keep every sent
    /// message at or below this; larger payloads have to be chunked. Reported per-channel so a
    /// constrained channel (which can negotiate a smaller limit) is respected.
    fn max_message_size(&self) -> usize;

    /// This is a debug method to dump the stats of webrtc connection.
    async fn get_stats(&self) -> Vec<String>;

    /// Return remote ICE-candidate IPs known before or after pair nomination.
    ///
    /// Native gateway routing uses this pre-connect projection to keep the transport underlay out
    /// of its own capture route. Backends without IP-addressed underlay candidates return empty.
    fn underlay_remote_ips(&self) -> Vec<IpAddr> {
        Vec::new()
    }

    /// Create a webrtc offer to start handshake.
    async fn webrtc_create_offer(&self) -> Result<Self::Sdp, Self::Error>;

    /// Accept a webrtc offer from remote peer and give back an answer.
    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp, Self::Error>;

    /// Accept a webrtc answer from remote peer.
    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<(), Self::Error>;

    /// Wait for the data channel to be opened after handshake.
    async fn webrtc_wait_for_data_channel_open(&self) -> Result<(), Self::Error>;

    /// Close the webrtc connection.
    async fn close(&self) -> Result<(), Self::Error>;
}

/// This trait specifies how to management [ConnectionInterface] objects.
/// Each platform must implement this trait for its own connection implementation.
/// See [connections](crate::connections) module for examples.
#[cfg_attr(all(feature = "web-sys-webrtc", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(
    not(all(feature = "web-sys-webrtc", target_family = "wasm")),
    async_trait
)]
pub trait TransportInterface {
    /// The connection type that is created by this trait.
    type Connection: ConnectionInterface<Error = Self::Error>;

    /// The error type that is returned by transport.
    type Error: std::error::Error;

    /// Return the stable raw-frame capacity account shared by every connection.
    ///
    /// Implementations must return the same allocation for their entire lifetime.
    fn inbound_frame_capacity(&self) -> &Arc<InboundFrameCapacity>;

    /// Used to create a new connection and register it in the transport.
    ///
    /// The returned weak reference identifies the exact physical connection
    /// inserted by this call. Callers must retain that identity across
    /// asynchronous cleanup instead of resolving the connection id again.
    ///
    /// See [connections](crate::connections) module for examples.
    async fn new_connection(
        &self,
        cid: &str,
        callback: BoxedTransportCallback,
    ) -> Result<ConnectionRef<Self::Connection>, Self::Error>;

    /// This method closes and releases the connection from transport.
    /// All references to this cid, created by `get_connection`, will be released.
    async fn close_connection(&self, cid: &str) -> Result<(), Self::Error>;

    /// Close `connection` only if it still owns its connection-id slot.
    ///
    /// This is the cleanup boundary for asynchronous work that may finish after
    /// another physical connection has replaced the observed connection.
    async fn close_connection_if_current(
        &self,
        connection: &ConnectionRef<Self::Connection>,
    ) -> Result<bool, Self::Error>;

    /// Get a reference of the connection by its id.
    fn connection(&self, cid: &str) -> Result<ConnectionRef<Self::Connection>, Self::Error>;

    /// Get all the connections in the transport.
    fn connections(&self) -> Vec<(String, ConnectionRef<Self::Connection>)>;

    /// Get all the connection ids in the transport.
    fn connection_ids(&self) -> Vec<String>;
}

/// Used to store a boxed [TransportInterface] trait object.
#[cfg(not(all(feature = "web-sys-webrtc", target_family = "wasm")))]
pub type BoxedTransport<C, E> =
    Box<dyn TransportInterface<Connection = C, Error = E> + Send + Sync>;

/// Used to store a boxed [TransportInterface] trait object.
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
pub type BoxedTransport<C, E> = Box<dyn TransportInterface<Connection = C, Error = E>>;
