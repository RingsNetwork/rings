//! The main entity of this module is the [ConnectionInterface] trait, which provides an
//! interface for establishing connections with other nodes, send data channel message to it.
//!
//! There is also a [TransportInterface] trait, which is used to specify the management of all
//! [ConnectionInterface] objects.

use std::sync::Arc;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::Mutex;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::MutexGuard;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use crate::callback::InboundFrameCapacity;
use crate::connection_ref::ConnectionRef;
use crate::core::admission::AdmissionEvent;
use crate::core::admission::AdmissionPhase;
use crate::core::admission::AtomicAdmission;
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

/// Maximum time a native backend drives an irrevocable send to completion.
pub const IRREVOCABLE_SEND_COMPLETION_TIMEOUT: Duration = Duration::from_secs(25);
/// Maximum cleanup interval after a connection generation becomes terminal.
pub const CONNECTION_RETIRE_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_family = "wasm")]
type SendPermitPredicate = dyn Fn() -> bool;
#[cfg(not(target_family = "wasm"))]
type SendPermitPredicate = dyn Fn() -> bool + Send + Sync;

#[cfg(target_family = "wasm")]
type SendPermitIrrevocableGuard = dyn for<'a> Fn(SendPermitClaim<'a>);
#[cfg(not(target_family = "wasm"))]
type SendPermitIrrevocableGuard = dyn for<'a> Fn(SendPermitClaim<'a>) + Send + Sync;

/// A one-send predicate checked at the backend's final cancellable send-admission boundary.
///
/// The permit is intentionally not `Clone`: one constructed value authorizes at
/// most one call to [`ConnectionInterface::send_message_with_permit`]. Returning
/// `false` means the higher-level condition that authorized the send no longer
/// holds, so the backend must not start its send primitive. The backend marks
/// acceptance only after its send primitive confirms queue admission.
pub struct SendPermit {
    predicate: Arc<SendPermitPredicate>,
    irrevocable_guard: Arc<SendPermitIrrevocableGuard>,
    state: AtomicAdmission,
}

/// One-use capability that linearizes backend admission with an external guard.
pub struct SendPermitClaim<'a> {
    state: &'a AtomicAdmission,
}

/// Proof that a backend crossed the final cancellation-safe send boundary.
pub struct IrrevocableSendPermit {
    state: AtomicAdmission,
}

/// Retires a connection generation when an irrevocable send does not reach acceptance.
#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    feature = "web-sys-webrtc",
    test
))]
pub(crate) struct IrrevocableSendGuard<F: FnOnce()> {
    acceptance: SendAcceptance,
    permit: Option<IrrevocableSendPermit>,
    retire: Option<F>,
}

/// Shared observation of whether a one-send permit reached its linearization point.
#[derive(Clone)]
pub struct SendAcceptance {
    state: AtomicAdmission,
}

impl SendAcceptance {
    /// Return whether the backend crossed its final cancellation-safe boundary.
    pub fn is_irrevocable(&self) -> bool {
        matches!(
            self.state.phase(),
            AdmissionPhase::Irrevocable | AdmissionPhase::Accepted
        )
    }

    /// Return whether the backend accepted the send permit.
    pub fn is_accepted(&self) -> bool {
        self.state.phase() == AdmissionPhase::Accepted
    }

    /// Return whether an irrevocable send failed before backend acceptance.
    pub fn failed_after_irrevocable(&self) -> bool {
        self.state.phase() == AdmissionPhase::Irrevocable
    }

    /// Atomically cancel a send that has not crossed its irrevocable boundary.
    pub fn try_cancel(&self) -> bool {
        self.state.try_transition(AdmissionEvent::Cancel).is_ok()
    }
}

impl SendPermitClaim<'_> {
    /// Claim the final cancellation-safe boundary while the caller's guards are held.
    pub fn try_claim(self) -> bool {
        self.state
            .try_transition(AdmissionEvent::MarkIrrevocable)
            .is_ok()
    }
}

impl SendPermit {
    /// Construct a send permit for a single-threaded wasm transport.
    #[cfg(target_family = "wasm")]
    pub fn new(predicate: impl Fn() -> bool + 'static) -> Self {
        Self {
            predicate: Arc::new(predicate),
            irrevocable_guard: Arc::new(|claim| {
                let _claimed = claim.try_claim();
            }),
            state: AtomicAdmission::new(),
        }
    }

    /// Construct a send permit for a native transport.
    #[cfg(not(target_family = "wasm"))]
    pub fn new(predicate: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            predicate: Arc::new(predicate),
            irrevocable_guard: Arc::new(|claim| {
                let _claimed = claim.try_claim();
            }),
            state: AtomicAdmission::new(),
        }
    }

    /// Construct an unconditional permit for direct low-level transport users.
    pub fn always() -> Self {
        Self::new(|| true)
    }

    /// Evaluate this permit where a backend is about to start its send.
    pub fn allows(&self) -> bool {
        (self.predicate)()
    }

    /// Add a final-boundary guard that calls `claim.try_claim()` only while its
    /// external invariants hold. A successful claim always produces the proof
    /// returned by [`Self::try_mark_irrevocable`].
    #[cfg(target_family = "wasm")]
    pub fn with_irrevocable_guard(
        mut self,
        guard: impl for<'a> Fn(SendPermitClaim<'a>) + 'static,
    ) -> Self {
        self.irrevocable_guard = Arc::new(guard);
        self
    }

    /// Add a final-boundary guard that calls `claim.try_claim()` only while its
    /// external invariants hold. A successful claim always produces the proof
    /// returned by [`Self::try_mark_irrevocable`].
    #[cfg(not(target_family = "wasm"))]
    pub fn with_irrevocable_guard(
        mut self,
        guard: impl for<'a> Fn(SendPermitClaim<'a>) + Send + Sync + 'static,
    ) -> Self {
        self.irrevocable_guard = Arc::new(guard);
        self
    }

    /// Evaluate the permit and cross the final cancellation-safe boundary.
    ///
    /// A backend must call this synchronously before its first non-cancellation-safe
    /// yield, write, or spawned task. After this returns a proof token, the write
    /// must be driven to completion while its connection remains usable. A caller
    /// may abandon the returned future only after permanently retiring that
    /// connection generation and initiating connection close.
    pub fn try_mark_irrevocable(self) -> Option<IrrevocableSendPermit> {
        if !self.allows() {
            return None;
        }
        (self.irrevocable_guard)(SendPermitClaim { state: &self.state });
        if self.state.phase() != AdmissionPhase::Irrevocable {
            return None;
        }
        Some(IrrevocableSendPermit { state: self.state })
    }

    /// Return a shared observer for the backend acceptance boundary.
    pub fn acceptance(&self) -> SendAcceptance {
        SendAcceptance {
            state: self.state.clone(),
        }
    }
}

impl IrrevocableSendPermit {
    /// Consume the proof and publish successful backend queue admission.
    pub fn mark_accepted(self) {
        let transitioned = self.state.try_transition(AdmissionEvent::Accept);
        debug_assert!(
            transitioned.is_ok(),
            "send acceptance requires irrevocable state"
        );
    }
}

#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    feature = "web-sys-webrtc",
    test
))]
impl<F: FnOnce()> IrrevocableSendGuard<F> {
    pub(crate) fn new(acceptance: SendAcceptance, retire: F) -> Self {
        Self {
            acceptance,
            permit: None,
            retire: Some(retire),
        }
    }

    pub(crate) fn bind(&mut self, permit: IrrevocableSendPermit) {
        self.permit = Some(permit);
    }

    pub(crate) fn mark_accepted(mut self) {
        if let Some(permit) = self.permit.take() {
            permit.mark_accepted();
        }
        self.retire = None;
    }
}

#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    feature = "web-sys-webrtc",
    test
))]
impl<F: FnOnce()> Drop for IrrevocableSendGuard<F> {
    fn drop(&mut self) {
        let must_retire = self.acceptance.failed_after_irrevocable();
        drop(self.permit.take());
        if must_retire {
            if let Some(retire) = self.retire.take() {
                retire();
            }
        }
    }
}

#[cfg(test)]
mod send_permit_tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use bytes::Bytes;
    use serde::Serialize;

    use super::IrrevocableSendGuard;
    use super::SendPermit;
    use super::TransportMessage;

    #[derive(Serialize)]
    enum LegacyTransportMessage {
        Custom(Vec<u8>),
    }

    #[test]
    fn send_permit_observes_revocation_at_evaluation_time() {
        let admitted = Arc::new(AtomicBool::new(true));
        admitted.store(false, Ordering::SeqCst);
        let permit = SendPermit::new({
            let admitted = Arc::clone(&admitted);
            move || admitted.load(Ordering::SeqCst)
        });

        assert!(!permit.allows());
    }

    #[test]
    fn send_acceptance_observes_confirmed_queue_admission() {
        let permit = SendPermit::always();
        let acceptance = permit.acceptance();

        assert!(!acceptance.is_accepted());
        assert!(!acceptance.is_irrevocable());
        assert!(!acceptance.failed_after_irrevocable());
        assert!(permit.allows());
        let permit = permit
            .try_mark_irrevocable()
            .expect("live permit must become irrevocable");
        assert!(acceptance.is_irrevocable());
        assert!(!acceptance.is_accepted());
        assert!(acceptance.failed_after_irrevocable());
        permit.mark_accepted();
        assert!(acceptance.is_accepted());
        assert!(acceptance.is_irrevocable());
        assert!(!acceptance.failed_after_irrevocable());
    }

    #[test]
    fn irrevocable_transition_is_one_shot_and_requires_a_live_predicate() {
        let denied = SendPermit::new(|| false);
        let denied_acceptance = denied.acceptance();
        assert!(denied.try_mark_irrevocable().is_none());
        assert!(!denied_acceptance.is_irrevocable());

        let admitted = SendPermit::always();
        let admitted_acceptance = admitted.acceptance();
        let _irrevocable = admitted
            .try_mark_irrevocable()
            .expect("live permit must become irrevocable");
        assert!(admitted_acceptance.is_irrevocable());
        assert!(!admitted_acceptance.is_accepted());
    }

    #[test]
    fn cancellation_and_irrevocable_admission_are_mutually_exclusive() {
        let cancelled = SendPermit::always();
        let cancelled_acceptance = cancelled.acceptance();
        assert!(cancelled_acceptance.try_cancel());
        assert!(cancelled.try_mark_irrevocable().is_none());
        assert!(!cancelled_acceptance.is_irrevocable());

        let admitted = SendPermit::always();
        let admitted_acceptance = admitted.acceptance();
        let _permit = admitted
            .try_mark_irrevocable()
            .expect("irrevocable admission must win before cancellation");
        assert!(!admitted_acceptance.try_cancel());
        assert!(admitted_acceptance.is_irrevocable());
    }

    #[test]
    fn irrevocable_guard_is_claimed_only_at_the_final_boundary() {
        let guard_open = Arc::new(AtomicBool::new(false));
        let permit = SendPermit::always().with_irrevocable_guard({
            let guard_open = Arc::clone(&guard_open);
            move |claim| {
                if guard_open.load(Ordering::SeqCst) {
                    let _claimed = claim.try_claim();
                }
            }
        });
        let denied_acceptance = permit.acceptance();

        assert!(permit.allows());
        assert!(!denied_acceptance.is_irrevocable());
        assert!(permit.try_mark_irrevocable().is_none());
        let permit = SendPermit::always().with_irrevocable_guard({
            let guard_open = Arc::clone(&guard_open);
            move |claim| {
                if guard_open.load(Ordering::SeqCst) {
                    let _claimed = claim.try_claim();
                }
            }
        });
        let admitted_acceptance = permit.acceptance();
        guard_open.store(true, Ordering::SeqCst);
        assert!(permit.try_mark_irrevocable().is_some());
        assert!(admitted_acceptance.is_irrevocable());
    }

    #[test]
    fn irrevocable_guard_cannot_forge_a_proof_without_claiming_this_permit() {
        let permit = SendPermit::always().with_irrevocable_guard(|_claim| {});
        let acceptance = permit.acceptance();

        assert!(permit.try_mark_irrevocable().is_none());
        assert!(acceptance.try_cancel());
        assert!(!acceptance.is_irrevocable());
    }

    #[test]
    fn claiming_in_a_guard_always_returns_the_matching_proof() {
        let permit = SendPermit::always().with_irrevocable_guard(|claim| {
            assert!(claim.try_claim());
        });
        let acceptance = permit.acceptance();

        let proof = permit
            .try_mark_irrevocable()
            .expect("a successful claim must return its proof");
        assert!(acceptance.is_irrevocable());
        proof.mark_accepted();
        assert!(acceptance.is_accepted());
    }

    #[test]
    fn failed_irrevocable_send_retires_while_acceptance_disarms_retirement() {
        let failed = SendPermit::always();
        let failed_acceptance = failed.acceptance();
        let failed_retired = Arc::new(AtomicBool::new(false));
        {
            let retired = Arc::clone(&failed_retired);
            let mut guard = IrrevocableSendGuard::new(failed_acceptance.clone(), move || {
                retired.store(true, Ordering::Release)
            });
            guard.bind(
                failed
                    .try_mark_irrevocable()
                    .expect("live permit must become irrevocable"),
            );
        }
        assert!(failed_acceptance.is_irrevocable());
        assert!(!failed_acceptance.is_accepted());
        assert!(failed_retired.load(Ordering::Acquire));

        let accepted = SendPermit::always();
        let accepted_observer = accepted.acceptance();
        let accepted_retired = Arc::new(AtomicBool::new(false));
        let mut guard = IrrevocableSendGuard::new(accepted_observer.clone(), {
            let retired = Arc::clone(&accepted_retired);
            move || retired.store(true, Ordering::Release)
        });
        guard.bind(
            accepted
                .try_mark_irrevocable()
                .expect("live permit must become irrevocable"),
        );
        guard.mark_accepted();
        assert!(accepted_observer.is_accepted());
        assert!(!accepted_retired.load(Ordering::Acquire));
    }

    #[test]
    fn claim_then_panic_retires_an_already_armed_send() {
        let permit = SendPermit::always().with_irrevocable_guard(|claim| {
            assert!(claim.try_claim());
            panic!("injected final-boundary guard panic");
        });
        let acceptance = permit.acceptance();
        let retired = Arc::new(AtomicBool::new(false));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let retired = Arc::clone(&retired);
            let guarded_acceptance = acceptance.clone();
            move || {
                let _retirement = IrrevocableSendGuard::new(guarded_acceptance, move || {
                    retired.store(true, Ordering::Release);
                });
                let _proof = permit.try_mark_irrevocable();
            }
        }));

        assert!(outcome.is_err());
        assert!(acceptance.is_irrevocable());
        assert!(!acceptance.is_accepted());
        assert!(retired.load(Ordering::Acquire));
    }

    #[test]
    fn bytes_transport_message_preserves_legacy_wire_encoding() {
        let body = vec![1, 2, 3, 4];
        let legacy = rings_codec::serialize(&LegacyTransportMessage::Custom(body.clone()))
            .expect("legacy message must serialize");
        let current = rings_codec::serialize(&TransportMessage::Custom(Bytes::from(body)))
            .expect("current message must serialize");

        assert_eq!(current, legacy);
    }

    #[test]
    fn custom_message_accepts_the_documented_vec_migration() {
        let body = vec![1, 2, 3, 4];
        let message: TransportMessage = TransportMessage::Custom(body.into());

        assert!(matches!(message, TransportMessage::Custom(bytes) if bytes.len() == 4));
    }
}

/// The state of the WebRTC connection.
/// This enum is used to define a same interface for all the platforms.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub enum WebrtcConnectionState {
    /// Unspecified
    #[default]
    Unspecified,

    /// WebrtcConnectionState::New indicates that any of the ICETransports or
    /// DTLSTransports are in the "new" state and none of the transports are
    /// in the "connecting", "checking", "failed" or "disconnected" state, or
    /// all transports are in the "closed" state, or there are no transports.
    New,

    /// WebrtcConnectionState::Connecting indicates that any of the
    /// ICETransports or DTLSTransports are in the "connecting" or
    /// "checking" state and none of them is in the "failed" state.
    Connecting,

    /// WebrtcConnectionState::Connected indicates that all ICETransports and
    /// DTLSTransports are in the "connected", "completed" or "closed" state
    /// and at least one of them is in the "connected" or "completed" state.
    Connected,

    /// WebrtcConnectionState::Disconnected indicates that any of the
    /// ICETransports or DTLSTransports are in the "disconnected" state
    /// and none of them are in the "failed" or "connecting" or "checking" state.
    Disconnected,

    /// WebrtcConnectionState::Failed indicates that any of the ICETransports
    /// or DTLSTransports are in a "failed" state.
    Failed,

    /// WebrtcConnectionState::Closed indicates the peer connection is closed
    /// and the isClosed member variable of PeerConnection is true.
    Closed,
}

impl WebrtcConnectionState {
    pub(crate) const fn occupies_peer_slot(self) -> bool {
        matches!(self, Self::New | Self::Connecting | Self::Connected)
    }

    #[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
    pub(crate) const fn is_terminal(self) -> bool {
        matches!(self, Self::Failed | Self::Closed)
    }
}

/// One coherent observation of the transport state used for routability.
///
/// The WebRTC and data-channel components are updated through one state cell,
/// so consumers never need to reconstruct this product from independent reads.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ConnectionStateSnapshot {
    webrtc: WebrtcConnectionState,
    data_channel_open: bool,
}

impl ConnectionStateSnapshot {
    /// Construct one transport-state observation.
    pub const fn new(webrtc: WebrtcConnectionState, data_channel_open: bool) -> Self {
        Self {
            webrtc,
            data_channel_open,
        }
    }

    /// Return the WebRTC peer-connection state.
    pub const fn webrtc(self) -> WebrtcConnectionState {
        self.webrtc
    }

    /// Return whether every outbound data channel is open.
    pub const fn data_channel_open(self) -> bool {
        self.data_channel_open
    }

    #[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
    fn apply(self, event: ConnectionStateEvent) -> Self {
        match event {
            ConnectionStateEvent::Close => Self::new(WebrtcConnectionState::Closed, false),
            ConnectionStateEvent::OutboundDataChannels(_) if self.webrtc.is_terminal() => self,
            ConnectionStateEvent::OutboundDataChannels(open) => Self::new(self.webrtc, open),
            ConnectionStateEvent::Webrtc(_) if self.webrtc == WebrtcConnectionState::Closed => self,
            ConnectionStateEvent::Webrtc(WebrtcConnectionState::Closed) => {
                Self::new(WebrtcConnectionState::Closed, false)
            }
            ConnectionStateEvent::Webrtc(_) if self.webrtc == WebrtcConnectionState::Failed => self,
            ConnectionStateEvent::Webrtc(WebrtcConnectionState::Failed) => {
                Self::new(WebrtcConnectionState::Failed, false)
            }
            ConnectionStateEvent::Webrtc(next) => Self::new(next, self.data_channel_open),
        }
    }
}

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
#[derive(Clone, Copy)]
enum ConnectionStateEvent {
    Webrtc(WebrtcConnectionState),
    OutboundDataChannels(bool),
    Close,
}

/// Shared event-maintained transport state for concrete backends.
///
/// Clone law: every clone references the same state cell; cloning does not
/// duplicate protocol state. `Closed` is absorbing, and terminal states can
/// never retain or regain an open data channel.
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
#[derive(Clone)]
pub(crate) struct ConnectionStateCell {
    state: Arc<Mutex<ConnectionStateSnapshot>>,
}

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
impl ConnectionStateCell {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ConnectionStateSnapshot::new(
                WebrtcConnectionState::New,
                false,
            ))),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ConnectionStateSnapshot> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn apply(&self, event: ConnectionStateEvent) {
        let mut state = self.lock();
        *state = state.apply(event);
    }

    pub(crate) fn observe_webrtc(&self, state: WebrtcConnectionState) {
        self.apply(ConnectionStateEvent::Webrtc(state));
    }

    pub(crate) fn observe_outbound_data_channels(&self, open: bool) {
        self.apply(ConnectionStateEvent::OutboundDataChannels(open));
    }

    pub(crate) fn close(&self) {
        self.apply(ConnectionStateEvent::Close);
    }

    pub(crate) fn snapshot(&self) -> ConnectionStateSnapshot {
        *self.lock()
    }
}

/// Interop ceiling for a single data-channel message, in bytes — RFC 8841's default
/// `max-message-size` (65536), the value a spec-compliant peer accepts when it advertises nothing
/// else. We treat it as a hard send ceiling: a sender never exceeds it regardless of what the
/// remote advertises, and a per-channel
/// [`max_message_size`](ConnectionInterface::max_message_size) may resolve to *less* (a constrained
/// peer) but never more. NOTE: this is the protocol default, not an independently verified property
/// of every backend's SCTP stack — a peer advertising a *larger* limit is still clamped to this.
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
    /// proof with [`IrrevocableSendPermit::mark_accepted`] before returning
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

    /// The maximum size, in bytes, of one message this connection can send — the channel's
    /// negotiated SCTP / data-channel `max_message_size`, capped at
    /// [`MAX_DATA_CHANNEL_MESSAGE_SIZE`] for cross-peer interop. A caller must keep every sent
    /// message at or below this; larger payloads have to be chunked. Reported per-channel so a
    /// constrained channel (which can negotiate a smaller limit) is respected.
    fn max_message_size(&self) -> usize;

    /// This is a debug method to dump the stats of webrtc connection.
    async fn get_stats(&self) -> Vec<String>;

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

#[cfg(test)]
mod tests {
    // SDP parsing (including section semantics) is tested in `crate::core::sdp`; these cover the
    // policy `effective_*` layers on top of it (default / no-limit / cap).
    use super::effective_max_message_size;
    #[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
    use super::ConnectionStateCell;
    #[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
    use super::ConnectionStateSnapshot;
    use super::WebrtcConnectionState;
    use super::MAX_DATA_CHANNEL_MESSAGE_SIZE;

    /// A data-channel SDP advertising `max-message-size:<value>` in the right media section.
    fn sdp_with(value: &str) -> String {
        format!(
            "v=0\r\n\
             m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
             a=max-message-size:{value}\r\n"
        )
    }

    #[test]
    fn effective_absent_defaults_to_cap() {
        let sdp = "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
        assert_eq!(
            effective_max_message_size(sdp),
            MAX_DATA_CHANNEL_MESSAGE_SIZE
        );
    }

    #[test]
    fn effective_zero_means_no_limit_uses_cap() {
        assert_eq!(
            effective_max_message_size(&sdp_with("0")),
            MAX_DATA_CHANNEL_MESSAGE_SIZE
        );
    }

    #[test]
    fn effective_smaller_value_is_honoured() {
        assert_eq!(effective_max_message_size(&sdp_with("16384")), 16384);
    }

    #[test]
    fn effective_larger_value_is_capped() {
        assert_eq!(
            effective_max_message_size(&sdp_with("1048576")),
            MAX_DATA_CHANNEL_MESSAGE_SIZE
        );
    }

    #[test]
    fn effective_exactly_cap_is_cap() {
        assert_eq!(
            effective_max_message_size(&sdp_with("65536")),
            MAX_DATA_CHANNEL_MESSAGE_SIZE
        );
    }

    #[test]
    fn only_negotiating_or_connected_states_occupy_the_peer_slot() {
        let cases = [
            (WebrtcConnectionState::Unspecified, false),
            (WebrtcConnectionState::New, true),
            (WebrtcConnectionState::Connecting, true),
            (WebrtcConnectionState::Connected, true),
            (WebrtcConnectionState::Disconnected, false),
            (WebrtcConnectionState::Failed, false),
            (WebrtcConnectionState::Closed, false),
        ];

        for (state, expected) in cases {
            assert_eq!(state.occupies_peer_slot(), expected, "state: {state:?}");
        }
    }

    #[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
    #[test]
    fn connection_state_cell_projects_each_complete_observed_state() {
        let state = ConnectionStateCell::new();
        assert_eq!(
            state.snapshot(),
            ConnectionStateSnapshot::new(WebrtcConnectionState::New, false)
        );

        state.observe_outbound_data_channels(true);
        assert_eq!(
            state.snapshot(),
            ConnectionStateSnapshot::new(WebrtcConnectionState::New, true)
        );

        state.observe_webrtc(WebrtcConnectionState::Connected);
        assert_eq!(
            state.snapshot(),
            ConnectionStateSnapshot::new(WebrtcConnectionState::Connected, true)
        );

        state.close();
        assert_eq!(
            state.snapshot(),
            ConnectionStateSnapshot::new(WebrtcConnectionState::Closed, false)
        );
        state.observe_outbound_data_channels(true);
        state.observe_webrtc(WebrtcConnectionState::Connected);
        assert_eq!(
            state.snapshot(),
            ConnectionStateSnapshot::new(WebrtcConnectionState::Closed, false),
            "late transport events cannot reopen a locally closed state"
        );
    }

    #[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
    #[test]
    fn failed_state_rejects_late_open_and_can_only_advance_to_closed() {
        let state = ConnectionStateCell::new();
        state.observe_outbound_data_channels(true);
        state.observe_webrtc(WebrtcConnectionState::Failed);
        assert_eq!(
            state.snapshot(),
            ConnectionStateSnapshot::new(WebrtcConnectionState::Failed, false)
        );

        state.observe_outbound_data_channels(true);
        state.observe_webrtc(WebrtcConnectionState::Connected);
        assert_eq!(
            state.snapshot(),
            ConnectionStateSnapshot::new(WebrtcConnectionState::Failed, false)
        );

        state.observe_webrtc(WebrtcConnectionState::Closed);
        assert_eq!(
            state.snapshot(),
            ConnectionStateSnapshot::new(WebrtcConnectionState::Closed, false)
        );
    }
}
