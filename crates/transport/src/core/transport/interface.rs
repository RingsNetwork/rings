use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use rings_runtime::MaybeSendSync;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use super::ConnectionStateSnapshot;
use super::LaneCreditView;
use super::SendPermit;
use super::TransportConnectionState;
use crate::connection_ref::ConnectionRef;
use crate::core::callback::BoxedTransportCallback;
use crate::core::credit::LaneSignal;
use crate::core::pool::ChannelLane;
use crate::delivery::DeliveryFuture;
use crate::delivery::SendCreditWait;
use crate::error::Error;
use crate::sdp::parse_sdp_max_message_size;

/// Wrapper for the data that is sent over the data channel.
///
/// Variant order is wire format: `Custom` keeps index 0, so a custom frame encodes as before.
///
/// Law (only the pump signals). The enum is private to the crate and the public send API
/// takes a custom payload, so a `Signal` frame leaves this end only from its own pump: every
/// limit the peer holds was advertised by this end's receive window, and every shrink and
/// return was made by its credit algebra.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub(crate) enum TransportMessage {
    /// A custom message sent by an external invoker and handled by the
    /// `on_admitted_message` callback. Since 0.18 this stores [`Bytes`]
    /// instead of `Vec<u8>` without changing its wire encoding.
    Custom(Bytes),
    /// A credit signal of the lane the frame travels on: a credit, a shrink or a return (see
    /// [`crate::core::credit`]). The transport applies it on arrival; it is never dispatched to
    /// the callback, never charged against credit itself, and idempotent per generation, so it
    /// may be duplicated or reordered.
    Signal(LaneSignal),
}

/// [`TransportMessage`] decoded without copying a custom payload.
#[derive(Deserialize)]
pub(crate) enum BorrowedTransportMessage<'a> {
    /// See [`TransportMessage::Custom`].
    Custom(#[serde(borrow)] &'a [u8]),
    /// See [`TransportMessage::Signal`].
    Signal(LaneSignal),
}

/// The wire frame a send of the custom `payload` puts on the data channel, for callers that
/// size or replay frames. Credit frames have no public encoder: only the transport sends them.
pub fn custom_frame_wire(payload: Bytes) -> rings_codec::Result<Vec<u8>> {
    rings_codec::serialize(&TransportMessage::Custom(payload))
}

/// Interop ceiling for a single transport channel message, in bytes (65536).
/// We treat it as a hard send ceiling: a sender never exceeds it regardless of what the
/// remote advertises, and a per-channel
/// [`max_message_size`](ConnectionInterface::max_message_size) may resolve to *less* (a constrained
/// peer) but never more.
pub const MAX_CHANNEL_MESSAGE_SIZE: usize = 65536;

/// Smallest negotiated `max_message_size` a connection admits (#1130).
///
/// A peer advertising a positive `max-message-size` below this floor would force the widest
/// honest DHT control frame (`ConnectNodeSend` or `ConnectNodeReport`) to be fragmented by the
/// chunk layer, or rejected if sent whole. Connections whose remote handshake description
/// advertises `0 < max-message-size < MIN_MAX_MESSAGE_SIZE` are refused before applying the
/// remote description.
///
/// Derivation: the widest control message is `ConnectNodeSend` / `ConnectNodeReport` carrying a
/// `MAX_HANDSHAKE_BYTES` (`4096`-byte) session description (`4111` message bytes), wrapped in the
/// widest `MessagePayload` envelope (`2219` bytes for a `< 16 KiB` message, whose 2-byte
/// `transaction.data` length prefix is one byte shorter than `MAX_PAYLOAD_ENVELOPE_OVERHEAD =
/// 2009`) and the outer transport frame reserve (`TRANSPORT_CUSTOM_OVERHEAD = 4`),
/// `4111 + 2008 + 4 = 6123`; witnessed with equality in `rings_core` by
/// `test_min_max_message_size_fits_widest_control_payloads_whole`.
pub const MIN_MAX_MESSAGE_SIZE: usize = 6123;

/// Decode the internal `0 = not negotiated` sentinel used by connection backends.
#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    feature = "web-sys-webrtc"
))]
pub(crate) const fn stored_max_message_size(stored: usize) -> usize {
    if stored == 0 {
        MAX_CHANNEL_MESSAGE_SIZE
    } else {
        stored
    }
}

/// The effective per-message send limit for a peer whose handshake description is `remote_handshake`.
/// An absent attribute defaults to 65536 and a value of `0` means "no limit" (still bounded by our
/// own send cap); any explicit value at or above [`MIN_MAX_MESSAGE_SIZE`] is honoured and capped at
/// [`MAX_CHANNEL_MESSAGE_SIZE`] for interop, while any positive value below [`MIN_MAX_MESSAGE_SIZE`]
/// is refused with [`Error::RemoteMaxMessageSizeTooSmall`] so DHT control is never chunked (#1130).
pub fn effective_max_message_size(remote_handshake: &str) -> crate::error::Result<usize> {
    match parse_sdp_max_message_size(remote_handshake) {
        None | Some(0) => Ok(MAX_CHANNEL_MESSAGE_SIZE),
        Some(n) if (n as usize) < MIN_MAX_MESSAGE_SIZE => {
            Err(Error::RemoteMaxMessageSizeTooSmall {
                advertised: n as usize,
                minimum: MIN_MAX_MESSAGE_SIZE,
            })
        }
        Some(n) => Ok((n as usize).min(MAX_CHANNEL_MESSAGE_SIZE)),
    }
}

/// The [ConnectionInterface] trait defines the abstract two-party offer/answer handshake and
/// credit-bounded framed message transport for a peer connection.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait ConnectionInterface {
    /// Handshake token used to exchange local and remote session descriptions when handshaking.
    type Handshake: Serialize + DeserializeOwned;
    /// The error type that is returned by connection.
    type Error: std::error::Error + From<Error>;

    /// Send the custom `payload` to the remote peer, for its `on_admitted_message` callback.
    ///
    /// The returned `Result` reflects whether the bytes were accepted into the
    /// local send buffer. The [DeliveryFuture] it yields resolves later to the
    /// message's actual fate: `Ok(())` once flushed to the wire, or `Err(..)`
    /// if the channel closed while the bytes were still buffered. Callers that
    /// don't care can drop it; callers that do can spawn it (see
    /// [crate::delivery]).
    ///
    /// It uses the default lane; see [`send_message_with_permit`](Self::send_message_with_permit).
    async fn send_message(&self, payload: Bytes) -> Result<DeliveryFuture, Self::Error> {
        self.send_message_with_permit(payload, ChannelLane::default(), SendPermit::always())
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
    ///
    /// The message travels on the channel `lane` is pinned to, so the messages of one lane
    /// reach the remote handler in send order (see [`crate::core::pool`]).
    ///
    /// The send is charged one credit of `lane` (see
    /// [`reserve_send_credit`](Self::reserve_send_credit)): the one `permit` carries, or else one
    /// reserved here.
    async fn send_message_with_permit(
        &self,
        payload: Bytes,
        lane: ChannelLane,
        permit: SendPermit,
    ) -> Result<DeliveryFuture, Self::Error>;

    /// Wait for one credit to send a custom frame on `lane` (see [`crate::core::credit`]).
    ///
    /// This is where a sender is held back while the receiver holds the lane's window full:
    /// the wait is backpressure, and lasts as long as the receiver keeps its window full. A
    /// caller that bounds its send in time reserves here first, untimed, and hands the credit to
    /// the send with [`SendPermit::with_credit`]. A send without a credit reserves one itself.
    ///
    /// The wait owns only the lane's credit state, never the connection, so a sender held back
    /// for long keeps no connection, callback or channel alive.
    fn reserve_send_credit(&self, lane: ChannelLane) -> SendCreditWait<Self::Error>;

    /// A read-only view of `lane`'s credit on this connection generation (see
    /// [`LaneCreditView`]): for a sender that shapes its own emission to the receiver's release
    /// rate, and for a receiver that reads the credit its peer holds unused. It holds the
    /// connection weakly and reserves nothing.
    fn send_credit_view(&self, lane: ChannelLane) -> LaneCreditView;

    /// Return one coherent connection-phase and channel-readiness product-state observation.
    fn connection_state_snapshot(&self) -> ConnectionStateSnapshot;

    /// Get current transport connection state: the phase component of
    /// [`connection_state_snapshot`](Self::connection_state_snapshot).
    fn connection_state(&self) -> TransportConnectionState {
        self.connection_state_snapshot().state()
    }

    /// Return whether every payload channel used by this connection is currently open: the
    /// channel-readiness component of [`connection_state_snapshot`](Self::connection_state_snapshot).
    ///
    /// This is one component of routability, not the complete predicate. The link phase
    /// may still report `Connecting` when channels have opened, while
    /// `Disconnected + Open` must not be treated as ready. Callers must classify
    /// the product state according to their protocol model.
    fn channel_is_open(&self) -> bool {
        self.connection_state_snapshot().channel_open()
    }

    /// The maximum size, in bytes, of one message this connection can send - the channel's
    /// negotiated `max_message_size`, capped at [`MAX_CHANNEL_MESSAGE_SIZE`] for cross-peer
    /// interop. A caller must keep every sent message at or below this; larger payloads have to be
    /// chunked. Reported per-channel so a constrained channel (which can negotiate a smaller limit)
    /// is respected.
    fn max_message_size(&self) -> usize;

    /// Create a local handshake offer to initiate a connection.
    async fn create_offer(&self) -> Result<Self::Handshake, Self::Error>;

    /// Accept a remote handshake offer and produce a local answer.
    async fn answer_offer(&self, offer: Self::Handshake) -> Result<Self::Handshake, Self::Error>;

    /// Accept a remote handshake answer from the peer.
    async fn accept_answer(&self, answer: Self::Handshake) -> Result<(), Self::Error>;

    /// Wait for the payload channel to become open after handshake completion.
    async fn wait_for_channel_open(&self) -> Result<(), Self::Error>;

    /// Close the connection.
    async fn close(&self) -> Result<(), Self::Error>;

    /// Capture a witness that survives pool removal and observes completed native physical close.
    #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
    fn physical_close_witness(
        &self,
    ) -> Result<crate::connections::NativePhysicalCloseWitness, Self::Error> {
        Err(Error::ChannelOpen(
            "physical_close_witness is only supported by WebrtcConnection".to_string(),
        )
        .into())
    }

    /// Stable physical generation identifier used by deterministic dummy simulations.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    fn dummy_generation_id(&self) -> Result<String, Self::Error> {
        Err(Error::ChannelOpen(
            "dummy_generation_id is only supported by DummyConnection".to_string(),
        )
        .into())
    }

    /// Test hook: force a dummy connection state without dispatching lifecycle callbacks.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    fn force_dummy_connection_state_without_callback(
        &self,
        _state: TransportConnectionState,
    ) -> Result<(), Self::Error> {
        Err(Error::ChannelOpen(
            "force_dummy_connection_state_without_callback is only supported by DummyConnection"
                .to_string(),
        )
        .into())
    }

    /// Test hook: override dummy channel readiness without dispatching callbacks.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    fn force_dummy_channel_open_without_callback(
        &self,
        _open: Option<bool>,
    ) -> Result<(), Self::Error> {
        Err(Error::ChannelOpen(
            "force_dummy_channel_open_without_callback is only supported by DummyConnection"
                .to_string(),
        )
        .into())
    }
}

/// Type-erased [`ConnectionInterface`] trait object (`Send + Sync` on native, unconstrained in
/// the browser).
pub type DynConnection =
    rings_runtime::maybe_send_sync!(dyn ConnectionInterface<Handshake = String, Error = Error>);

/// This trait specifies how to management [ConnectionInterface] objects.
/// Each platform must implement this trait for its own connection implementation.
/// See [connections](crate::connections) module for examples.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait TransportInterface {
    /// The connection type that is created by this trait.
    type Connection: ConnectionInterface<Error = Self::Error> + MaybeSendSync;

    /// The error type that is returned by transport.
    type Error: std::error::Error;

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

    /// Close `connection` only if it still owns its connection-id slot.
    ///
    /// This is the cleanup boundary for asynchronous work that may finish after
    /// another physical connection has replaced the observed connection.
    async fn close_connection_if_current(
        &self,
        connection: &ConnectionRef<Self::Connection>,
    ) -> Result<bool, Self::Error>;

    /// Close a type-erased `connection` reference only if its physical allocation still owns its
    /// connection-id slot.
    async fn close_dyn_connection_if_current(
        &self,
        connection: &ConnectionRef<DynConnection>,
    ) -> Result<bool, Self::Error> {
        let Ok(current) = self.connection(connection.cid()) else {
            return Ok(false);
        };
        if !connection.same_allocation(&current) {
            return Ok(false);
        }
        self.close_connection_if_current(&current).await
    }

    /// Get a reference of the connection by its id.
    fn connection(&self, cid: &str) -> Result<ConnectionRef<Self::Connection>, Self::Error>;

    /// Get all the connection ids in the transport.
    fn connection_ids(&self) -> Vec<String>;

    /// Update the transport's active session-bound traversal/relay credentials.
    fn update_session_credentials(&self, _credentials: &str) {}

    /// Return the transport's currently configured session-bound traversal/relay credentials, if any.
    fn session_credentials(&self) -> Option<String> {
        None
    }

    /// Return the total custom payload frames dispatched by this transport (dummy backend).
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    fn dispatched_payload_frames(&self) -> usize {
        0
    }
}

/// Object-safe, backend-agnostic transport interface consumed by higher protocol layers
/// (`rings-core`) without depending on concrete transport connection types.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait DynTransportInterface {
    /// Create a new connection and register it in the transport, returning a weak erased
    /// reference that preserves exact physical allocation identity.
    async fn new_dyn_connection(
        &self,
        cid: &str,
        callback: BoxedTransportCallback,
    ) -> Result<ConnectionRef<DynConnection>, Error>;

    /// Close `connection` only if its physical allocation still owns its connection-id slot.
    async fn close_dyn_connection_if_current(
        &self,
        connection: &ConnectionRef<DynConnection>,
    ) -> Result<bool, Error>;

    /// Get a weak erased reference to the connection by its id.
    fn dyn_connection(&self, cid: &str) -> Result<ConnectionRef<DynConnection>, Error>;

    /// Get all connection ids currently in the transport.
    fn connection_ids(&self) -> Vec<String>;

    /// Validate a remote handshake offer description before opening or yielding a peer slot.
    fn validate_remote_offer(&self, remote_handshake: &str) -> Result<(), Error> {
        effective_max_message_size(remote_handshake).map(|_| ())
    }

    /// Update the transport's active session-bound traversal/relay credentials.
    fn update_session_credentials(&self, _credentials: &str) {}

    /// Return the transport's currently configured session-bound traversal/relay credentials, if any.
    fn session_credentials(&self) -> Option<String> {
        None
    }

    /// Return the total custom payload frames dispatched by this transport (dummy backend).
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    fn dispatched_payload_frames(&self) -> usize {
        0
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl<T, C> DynTransportInterface for T
where
    T: TransportInterface<Connection = C, Error = Error> + MaybeSendSync,
    C: ConnectionInterface<Handshake = String, Error = Error> + MaybeSendSync + 'static,
{
    async fn new_dyn_connection(
        &self,
        cid: &str,
        callback: BoxedTransportCallback,
    ) -> Result<ConnectionRef<DynConnection>, Error> {
        self.new_connection(cid, callback)
            .await
            .map(ConnectionRef::into_dyn)
    }

    async fn close_dyn_connection_if_current(
        &self,
        connection: &ConnectionRef<DynConnection>,
    ) -> Result<bool, Error> {
        TransportInterface::close_dyn_connection_if_current(self, connection).await
    }

    fn dyn_connection(&self, cid: &str) -> Result<ConnectionRef<DynConnection>, Error> {
        self.connection(cid).map(ConnectionRef::into_dyn)
    }

    fn connection_ids(&self) -> Vec<String> {
        TransportInterface::connection_ids(self)
    }

    fn update_session_credentials(&self, credentials: &str) {
        TransportInterface::update_session_credentials(self, credentials);
    }

    fn session_credentials(&self) -> Option<String> {
        TransportInterface::session_credentials(self)
    }

    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    fn dispatched_payload_frames(&self) -> usize {
        TransportInterface::dispatched_payload_frames(self)
    }
}

/// Shared, reference-counted object-safe transport handle injected into protocol layers.
pub type SharedTransport = Arc<rings_runtime::maybe_send_sync!(dyn DynTransportInterface)>;

/// Used to store a boxed [TransportInterface] trait object.
#[cfg(not(target_family = "wasm"))]
pub type BoxedTransport<C, E> =
    Box<dyn TransportInterface<Connection = C, Error = E> + Send + Sync>;

/// Used to store a boxed [TransportInterface] trait object.
#[cfg(target_family = "wasm")]
pub type BoxedTransport<C, E> = Box<dyn TransportInterface<Connection = C, Error = E>>;
