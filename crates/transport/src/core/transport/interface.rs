use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use rings_runtime::MaybeSendSync;
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

/// Wrapper for framed messages carried on a transport lane.
///
/// Variant order is wire format: `Custom` keeps index 0, so a custom frame encodes identically.
///
/// Law (only the pump signals). The enum is private to the crate and the public send API
/// takes a custom payload, so a `Signal` frame leaves this end only from its own pump: every
/// limit the peer holds was advertised by this end's receive window, and every shrink and
/// return was made by its credit algebra.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub(crate) enum TransportMessage {
    /// A custom message sent by an external invoker and handled by the
    /// `on_admitted_message` callback.
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

/// The wire frame a send of the custom `payload` puts on a transport lane, for callers that
/// size or replay frames. Credit frames have no public encoder: only the transport sends them.
pub fn custom_frame_wire(payload: Bytes) -> rings_codec::Result<Vec<u8>> {
    rings_codec::serialize(&TransportMessage::Custom(payload))
}

/// Interop ceiling for a single transport frame, in bytes (`65536`).
///
/// Treated as a hard send ceiling: a sender never exceeds it regardless of what the remote peer
/// advertises, and a per-connection [`max_message_size`](ConnectionInterface::max_message_size)
/// may resolve to less (for constrained media) so long as it meets [`MIN_MAX_MESSAGE_SIZE`].
pub const MAX_CHANNEL_MESSAGE_SIZE: usize = 65536;

/// Default and protocol-ceiling maximum byte size of a single handshake step payload (`4096`).
pub const DEFAULT_MAX_HANDSHAKE_STEP_BYTES: usize = 4096;

/// Maximum number of signaling steps permitted in a multi-step transport handshake (`8`).
pub const MAX_HANDSHAKE_STEPS: u8 = 8;

/// Smallest `max_message_size` a transport connection may expose (`6145` bytes, #1130 / #1291 / #1315).
///
/// Every connection must fit the widest unchunked DHT control frame (`ConnectNodeSend` or
/// `ConnectNodeReport`) whole without chunk fragmentation:
/// - Variant tag (`1`) + `step_index` (`1`) + `final_step` (`1`) + `session_id: Some([u8; 16])`
///   (`17`) + `handshake` length varint (`2`) + `DEFAULT_MAX_HANDSHAKE_STEP_BYTES` (`4096`) +
///   widest `DhtProtocolMode` (`11`) + `RelayCustody` (`1`) = `4130` message bytes;
/// - Plus the widest `< 16 KiB` `MessagePayload` envelope (`2011` bytes) and outer custom frame
///   header (`4` bytes): `4130 + 2011 + 4 = 6145`, witnessed with equality in `rings_core` by
///   `test_min_max_message_size_fits_widest_control_payloads_whole`.
pub const MIN_MAX_MESSAGE_SIZE: usize = 6145;

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

/// One step outcome of [`ConnectionInterface::handshake_step`] in a finite-round transport
/// handshake (#1291).
///
/// Supports arbitrary step counts without protocol-specific assumptions in `rings-core`:
/// - **0-step (pre-provisioned / connectionless)**: initiator returns [`Self::Done`] on
///   `handshake_step(None)`.
/// - **1-step (unidirectional announce)**: initiator returns [`Self::SendFinal`], responder
///   returns [`Self::Done`].
/// - **2-step (offer / answer)**: initiator returns [`Self::Send`] then [`Self::Done`]; responder
///   returns [`Self::SendFinal`].
/// - **3-step (Noise XX / mutual challenge)**: initiator returns [`Self::Send`] (`m1`) then
///   [`Self::SendFinal`] (`m3`); responder returns [`Self::Send`] (`m2`) then [`Self::Done`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandshakeStep {
    /// Transmit these raw handshake bytes to the peer; at least one inbound reply step is still
    /// expected before this side's handshake completes.
    Send(Bytes),
    /// Transmit these final handshake bytes to the peer; this side expects no further inbound
    /// handshake message. Responder final steps (`step_index >= 1`) and multi-round initiator
    /// final steps (`step_index >= 2`) may transition to channel readiness immediately; a 1-step
    /// initial offer (`step_index == 0`) should report `channel_open = true` once the remote peer
    /// establishes the link upon processing step 0 so crossing 1-step offers can tie-break cleanly.
    SendFinal(Bytes),
    /// This side's handshake is complete with no outbound handshake message to transmit.
    Done,
}

/// Abstract multi-step handshake and credit-bounded framed message transport for a peer link.
///
/// # Transport Capabilities & Credit Law
///
/// Every Rings connection multiplexes four ordered [`ChannelLane`]s governed by cumulative
/// receiver-advertised credit windows ([`crate::core::credit`]). Consequently, a transport
/// carrying more than one initial credit window (`INBOUND_PEER_FRAME_CAPACITY` frames) MUST
/// provide a **duplex reverse path** so credit-replenishment signals return from receiver to
/// sender, and MUST satisfy `max_message_size() >= MIN_MAX_MESSAGE_SIZE`.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait ConnectionInterface: 'static {
    /// The error type returned by connection operations.
    type Error: std::error::Error + From<Error>;

    /// Upcast this connection to `&dyn Any` for backend-owned extension downcasting without
    /// polluting the core object-safe trait with backend-specific methods.
    fn as_any(&self) -> &dyn Any;

    /// Send the custom `payload` to the remote peer on the default lane, for its
    /// `on_admitted_message` callback.
    ///
    /// The returned `Result` reflects whether the bytes were accepted into the local send buffer.
    /// The [`DeliveryFuture`] resolves later to `Ok(())` once flushed to the link or `Err(..)` if
    /// the link closed while buffered.
    async fn send_message(&self, payload: Bytes) -> Result<DeliveryFuture, Self::Error> {
        self.send_message_with_permit(payload, ChannelLane::default(), SendPermit::always())
            .await
    }

    /// Send `payload` on `lane` only if `permit` holds at the final cancellable backend boundary.
    ///
    /// External backends should delegate directly to
    /// [`crate::callback::InnerTransportCallback::send_custom_wire`], which enforces lane credit
    /// reservation, `max_message_size`, [`SendPermit::try_mark_irrevocable`], and post-irrevocable
    /// generation retirement by type construction.
    async fn send_message_with_permit(
        &self,
        payload: Bytes,
        lane: ChannelLane,
        permit: SendPermit,
    ) -> Result<DeliveryFuture, Self::Error>;

    /// Wait for one credit to send a custom frame on `lane` (see [`crate::core::credit`]).
    ///
    /// The wait owns only the lane's credit state, never the connection, so a sender held back
    /// under backpressure keeps no connection, callback, or channel alive.
    fn reserve_send_credit(&self, lane: ChannelLane) -> SendCreditWait<Self::Error>;

    /// Return a read-only view of `lane`'s credit on this connection generation.
    fn send_credit_view(&self, lane: ChannelLane) -> LaneCreditView;

    /// Return one coherent `(phase, channel_open)` product-state snapshot.
    fn connection_state_snapshot(&self) -> ConnectionStateSnapshot;

    /// Return the current connection phase from [`Self::connection_state_snapshot`].
    fn connection_state(&self) -> TransportConnectionState {
        self.connection_state_snapshot().state()
    }

    /// Return whether every outbound payload lane on this connection is currently open.
    fn channel_is_open(&self) -> bool {
        self.connection_state_snapshot().channel_open()
    }

    /// Return the maximum wire frame size in bytes this connection can transmit in a single frame
    /// (`MIN_MAX_MESSAGE_SIZE <= max_message_size <= MAX_CHANNEL_MESSAGE_SIZE`).
    fn max_message_size(&self) -> usize;

    /// Advance this connection's handshake state machine by one step.
    ///
    /// - Pass `inbound = None` on the initiator to start step `0`.
    /// - Pass `inbound = Some(bytes)` upon receiving a peer handshake payload to advance and
    ///   produce the next [`HandshakeStep`].
    async fn handshake_step(&self, inbound: Option<Bytes>) -> Result<HandshakeStep, Self::Error>;

    /// Convenience adapter for 2-step offer/answer callers: start initiator step `0` and return
    /// the non-final outbound offer bytes.
    async fn create_offer(&self) -> Result<Bytes, Self::Error> {
        match self.handshake_step(None).await? {
            HandshakeStep::Send(bytes) => Ok(bytes),
            HandshakeStep::SendFinal(_) | HandshakeStep::Done => {
                Err(Error::LocalHandshakeGenerationError(
                    "expected HandshakeStep::Send on 2-step create_offer".to_string(),
                )
                .into())
            }
        }
    }

    /// Convenience adapter for 2-step offer/answer callers: feed `offer` into step `1` and return
    /// the responder's final outbound answer bytes.
    async fn answer_offer(&self, offer: Bytes) -> Result<Bytes, Self::Error> {
        match self.handshake_step(Some(offer)).await? {
            HandshakeStep::SendFinal(bytes) => Ok(bytes),
            HandshakeStep::Send(_) | HandshakeStep::Done => {
                Err(Error::LocalHandshakeGenerationError(
                    "expected HandshakeStep::SendFinal on 2-step answer_offer".to_string(),
                )
                .into())
            }
        }
    }

    /// Convenience adapter for 2-step offer/answer callers: feed `answer` into the initiator to
    /// complete a 2-step handshake.
    async fn accept_answer(&self, answer: Bytes) -> Result<(), Self::Error> {
        match self.handshake_step(Some(answer)).await? {
            HandshakeStep::Done => Ok(()),
            HandshakeStep::Send(_) | HandshakeStep::SendFinal(_) => {
                Err(Error::LocalHandshakeGenerationError(
                    "expected HandshakeStep::Done when completing 2-step answer".to_string(),
                )
                .into())
            }
        }
    }

    /// Wait until all payload lanes become open after handshake completion.
    async fn wait_for_channel_open(&self) -> Result<(), Self::Error>;

    /// Close and permanently retire this connection generation.
    async fn close(&self) -> Result<(), Self::Error>;
}

/// Type-erased [`ConnectionInterface`] trait object (`Send + Sync` on native, unconstrained in
/// the browser).
pub type DynConnection = rings_runtime::maybe_send_sync!(dyn ConnectionInterface<Error = Error>);

/// Factory and pool manager for [`ConnectionInterface`] instances.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait TransportInterface: 'static {
    /// Concrete connection type managed by this transport.
    type Connection: ConnectionInterface<Error = Self::Error> + MaybeSendSync;

    /// Error type returned by transport operations.
    type Error: std::error::Error;

    /// Create a new connection for `cid` and register it in this transport's pool.
    ///
    /// The returned weak [`ConnectionRef`] identifies the exact physical connection generation
    /// inserted by this call.
    async fn new_connection(
        &self,
        cid: &str,
        callback: BoxedTransportCallback,
    ) -> Result<ConnectionRef<Self::Connection>, Self::Error>;

    /// Close `connection` only if its physical allocation still owns its connection-id slot.
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

    /// Look up the active connection reference for `cid`.
    fn connection(&self, cid: &str) -> Result<ConnectionRef<Self::Connection>, Self::Error>;

    /// List all connection identifiers currently held in this transport's pool.
    fn connection_ids(&self) -> Vec<String>;

    /// Maximum byte length permitted for a single handshake step payload on this transport,
    /// bounded above by [`DEFAULT_MAX_HANDSHAKE_STEP_BYTES`].
    fn max_handshake_step_bytes(&self) -> usize {
        DEFAULT_MAX_HANDSHAKE_STEP_BYTES
    }

    /// Whether this transport requires a duplex reverse path on each lane to return cumulative
    /// credit signals. All standard framed transports return `true`.
    fn requires_duplex_reverse_path(&self) -> bool {
        true
    }

    /// Validate a raw inbound handshake payload for `step_index` before allocating or mutating a
    /// peer slot. Invoked symmetrically on both initial offers (`step_index == 0`) and subsequent
    /// reply/continuation steps (`step_index >= 1`).
    fn validate_inbound_handshake(
        &self,
        _step_index: u8,
        _payload: &[u8],
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Object-safe transport trait consumed by `rings-core` (`SharedTransport`).
///
/// Blanket-implemented for every `TransportInterface<Error = Error>`.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait DynTransportInterface: 'static {
    /// Upcast this transport to `&dyn Any` so concrete `NodeNetworkParameters` or test harnesses
    /// can downcast to their own transport implementation without leaking backend methods onto
    /// `DynTransportInterface`.
    fn as_any(&self) -> &dyn Any;

    /// Allocate and register a new type-erased connection for `cid`.
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

    /// Look up the active type-erased connection reference for `cid`.
    fn dyn_connection(&self, cid: &str) -> Result<ConnectionRef<DynConnection>, Error>;

    /// Return all active connection IDs currently tracked by this transport.
    fn connection_ids(&self) -> Vec<String>;

    /// Maximum byte length permitted for a single handshake step payload on this transport,
    /// clamped to `[1, DEFAULT_MAX_HANDSHAKE_STEP_BYTES]`.
    fn max_handshake_step_bytes(&self) -> usize {
        DEFAULT_MAX_HANDSHAKE_STEP_BYTES
    }

    /// Whether this transport requires a duplex reverse path for credit signals.
    fn requires_duplex_reverse_path(&self) -> bool {
        true
    }

    /// Validate an inbound handshake step payload before mutating peer slot state.
    fn validate_inbound_handshake(&self, _step_index: u8, _payload: &[u8]) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl<T, C> DynTransportInterface for T
where
    T: TransportInterface<Connection = C, Error = Error> + MaybeSendSync + 'static,
    C: ConnectionInterface<Error = Error> + MaybeSendSync + 'static,
{
    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn new_dyn_connection(
        &self,
        cid: &str,
        callback: BoxedTransportCallback,
    ) -> Result<ConnectionRef<DynConnection>, Error> {
        let conn = TransportInterface::new_connection(self, cid, callback).await?;
        Ok(conn.into_dyn())
    }

    async fn close_dyn_connection_if_current(
        &self,
        connection: &ConnectionRef<DynConnection>,
    ) -> Result<bool, Error> {
        TransportInterface::close_dyn_connection_if_current(self, connection).await
    }

    fn dyn_connection(&self, cid: &str) -> Result<ConnectionRef<DynConnection>, Error> {
        TransportInterface::connection(self, cid).map(ConnectionRef::into_dyn)
    }

    fn connection_ids(&self) -> Vec<String> {
        TransportInterface::connection_ids(self)
    }

    fn max_handshake_step_bytes(&self) -> usize {
        TransportInterface::max_handshake_step_bytes(self)
            .clamp(1, DEFAULT_MAX_HANDSHAKE_STEP_BYTES)
    }

    fn requires_duplex_reverse_path(&self) -> bool {
        TransportInterface::requires_duplex_reverse_path(self)
    }

    fn validate_inbound_handshake(&self, step_index: u8, payload: &[u8]) -> Result<(), Error> {
        TransportInterface::validate_inbound_handshake(self, step_index, payload)
    }
}

/// Shared object-safe transport handle (`Arc<dyn DynTransportInterface + Send + Sync>` on native,
/// `Arc<dyn DynTransportInterface>` in the browser).
pub type SharedTransport = Arc<rings_runtime::maybe_send_sync!(dyn DynTransportInterface)>;

/// Boxed typed transport handle.
#[cfg(not(target_family = "wasm"))]
pub type BoxedTransport<C, E> =
    Box<dyn TransportInterface<Connection = C, Error = E> + Send + Sync>;

/// Boxed typed transport handle (single-threaded browser target).
#[cfg(target_family = "wasm")]
pub type BoxedTransport<C, E> = Box<dyn TransportInterface<Connection = C, Error = E>>;
