//! Error types returned by transport backends and transport utilities.

/// Transport result type using [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced while parsing ICE server configuration.
#[derive(thiserror::Error, Debug)]
pub enum IceServerError {
    /// Url parse error
    #[error("Url parse error")]
    UrlParse(#[from] url::ParseError),

    /// Ice server scheme {0} has not supported yet
    #[error("Ice server scheme {0} has not supported yet")]
    SchemeNotSupported(String),

    /// Cannot extract host from url
    #[error("Cannot extract host from url")]
    UrlMissHost,

    /// ICE server URL contains interior ASCII control characters (`\t`, `\r`, `\n`, etc.)
    #[error("ICE server URL contains ASCII control characters")]
    InvalidControlCharacter,
}

/// Errors produced by transport connections, pools, and backend adapters.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
    /// WebRTC error: {0}
    #[error("WebRTC error: {0}")]
    Webrtc(#[from] webrtc::error::Error),

    #[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
    /// WebSysWebRTC error: {}
    #[error("WebSysWebRTC error: {}", dump_js_value(.0))]
    WebSysWebrtc(wasm_bindgen::JsValue),

    /// Codec error: {0}
    #[error("Codec error: {0}")]
    Codec(#[from] rings_codec::Error),

    /// IceServer error: {0}
    #[error("IceServer error: {0}")]
    IceServer(#[from] IceServerError),

    /// Failed when waiting for channel open: {0}
    #[error("Failed when waiting for channel open: {0}")]
    ChannelOpen(String),

    /// The runtime could not run a timer (browser only).
    #[error("Timer error: {0}")]
    Timer(#[from] rings_runtime::TimerError),

    /// Message was not delivered: {0}
    #[error("Message was not delivered: {0}")]
    MessageNotDelivered(String),

    /// A channel message used an unsupported representation: {0}
    #[error("Unsupported channel message: {0}")]
    ChannelMessage(String),

    /// The higher-level authorization was revoked before backend send admission.
    #[error("Send permit was revoked before transport send admission")]
    SendPermitRevoked,

    /// Encoded size or cumulative channel byte offset exceeds the accounting domain.
    #[error("Send byte accounting exceeds u64")]
    SendByteCountOverflow,

    /// An outbound wire frame (`{size}` bytes) exceeded the connection's `max_message_size` (`{max}` bytes).
    #[error("Outbound transport frame of {size} bytes exceeds max_message_size {max}")]
    FrameTooLarge {
        /// Encoded wire frame size in bytes.
        size: usize,
        /// Negotiated or configured `max_message_size` in bytes.
        max: usize,
    },

    /// External custom transport backend failure paired with its post-admission send class.
    #[error("Custom transport backend error ({class:?}): {source}")]
    Backend {
        /// Whether the failure is ambiguous (post-irrevocable) or fatal (invariant/config).
        class: BackendSendFailureClass,
        /// Underlying backend error cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
    /// No Tokio runtime is available to drive a native data-channel write.
    #[error("Native data-channel send requires an active Tokio runtime")]
    NativeSendRuntimeUnavailable,

    #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
    /// An irrevocable native data-channel write exceeded its completion bound.
    #[error("Native data-channel send did not complete within {timeout_ms}ms")]
    NativeSendCompletionTimeout {
        /// Completion bound applied after the write became irrevocable.
        timeout_ms: u128,
    },

    #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
    /// A native task driving an irrevocable data-channel write stopped unexpectedly.
    #[error("Native data-channel send task stopped: {0}")]
    NativeSendTask(#[source] tokio::task::JoinError),

    #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
    /// A native data-channel send panicked while still owned by its caller.
    #[error("Native data-channel send panicked: {0}")]
    NativeSendPanic(String),

    #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
    /// A native task driving physical connection close stopped unexpectedly.
    #[error("Native connection close task stopped: {0}")]
    NativeConnectionCloseTask(#[source] tokio::task::JoinError),

    #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
    /// A native connection did not close within its bounded retirement interval.
    #[error("Native connection retirement did not complete within {timeout_ms}ms")]
    NativeConnectionRetirementTimeout {
        /// Retirement bound in milliseconds.
        timeout_ms: u128,
    },

    #[cfg(feature = "dummy")]
    /// The dummy connection retired before an irrevocable frame could be dispatched.
    #[error("Dummy connection retired before irrevocable dispatch")]
    DummyConnectionRetiredBeforeDispatch,

    #[cfg(feature = "dummy")]
    /// The paired dummy connection was unavailable before dispatch.
    #[error("Dummy remote connection is unavailable before dispatch")]
    DummyRemoteConnectionUnavailable,

    #[cfg(feature = "dummy")]
    /// The paired dummy connection stopped accepting events before dispatch.
    #[error("Dummy remote connection is closed")]
    DummyRemoteConnectionClosed,

    #[cfg(feature = "dummy")]
    /// The task driving an irrevocable dummy send stopped before publishing its result.
    #[error("Dummy irrevocable send task stopped before completion")]
    DummyIrrevocableSendTaskStopped,

    /// Local transport handshake generation error: {0}
    #[error("Local transport handshake generation error: {0}")]
    LocalHandshakeGenerationError(String),

    /// Remote transport handshake validation or application error: {0}
    #[error("Remote transport handshake error: {0}")]
    RemoteHandshakeApplyError(String),

    /// Operation `{0}` is not supported by the active concrete transport backend.
    #[error("Transport backend does not support operation `{0}`")]
    UnsupportedOperation(String),

    /// The remote peer advertised `max_message_size = {advertised}`, below the protocol floor
    /// `{minimum}` needed to carry every DHT control message unchunked.
    #[error(
        "Remote transport max-message-size {advertised} is below the minimum {minimum} required for unchunked DHT control"
    )]
    RemoteMaxMessageSizeTooSmall {
        /// Positive `max_message_size` advertised by the remote peer.
        advertised: usize,
        /// Minimum `max_message_size` required (`MIN_MAX_MESSAGE_SIZE`).
        minimum: usize,
    },

    /// WebRTC UDP port range was rejected by the ICE stack: {0}
    #[error("WebRTC UDP port range was rejected by the ICE stack: {0}")]
    UdpPortRange(String),

    /// Connection {0} already exists
    #[error("Connection {0} already exists")]
    ConnectionAlreadyExists(String),

    /// Connection {0} not found, should handshake first
    #[error("Connection {0} not found, should handshake first")]
    ConnectionNotFound(String),

    /// Connection {0} is released
    #[error("Connection {0} is released")]
    ConnectionReleased(String),

    /// Rwlock try write failed: {0}
    #[error("Rwlock try write failed: {0}")]
    RwLockWrite(String),

    /// Rwlock try read failed: {0}
    #[error("Rwlock try read failed: {0}")]
    RwLockRead(String),

    /// The connection to {0} closed while a send waited for credit.
    #[error("The connection to {0} closed while a send waited for credit")]
    LinkCreditClosed(String),

    /// A send carried a credit reserved on another connection than {0}'s.
    #[error("A send to {0} carried a credit reserved on another connection")]
    ForeignCredit(String),

    /// The connection to {0} cannot run its credit pump: no runtime.
    #[error("The connection to {0} cannot run its credit pump: no runtime")]
    CreditPumpUnavailable(String),

    /// Cannot select from an empty channel pool
    #[error("Cannot select from an empty channel pool")]
    ChannelPoolEmpty,
}

/// Serialize a [`wasm_bindgen::JsValue`] into a JSON string for error formatting.
#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
fn dump_js_value(v: &wasm_bindgen::JsValue) -> String {
    let Ok(s) = js_sys::JSON::stringify(v) else {
        return "Failed to stringify Error(JsValue)".to_string();
    };
    s.into()
}

/// The acceptance class of a transport send error, partitioning every backend error by whether
/// the frame could have crossed the irrevocable backend send boundary.
///
/// Laws:
/// - **Pre-acceptance soundness (Lemma (T))**: [`Self::PreAcceptance`] is returned iff the error
///   is [`Error::SendPermitRevoked`] or [`Error::LinkCreditClosed`], which are proven to occur
///   before [`crate::core::transport::SendPermit::try_mark_irrevocable`] is claimed.
/// - **Totality**: [`Error::send_failure_class`] is a total `const fn` over every compiled
///   variant of [`Error`] with no wildcard arm.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TransportSendFailureClass {
    /// The send failed strictly before backend acceptance; no byte of the frame was sent, so the
    /// caller may safely defer and retry on a link change.
    PreAcceptance,
    /// The send failed after or during an irrevocable backend write; the frame may or may not
    /// have been transmitted.
    Ambiguous,
    /// Configuration, pairing, or pre-dispatch invariant failure that cannot be retried.
    Fatal,
}

/// Post-admission failure classification available to external custom [`Error::Backend`]
/// implementations.
///
/// Pre-acceptance errors (`SendPermitRevoked` and `LinkCreditClosed`) are reserved exclusively for
/// pre-claim paths inside `rings-transport`, preserving Lemma (T) (`PreAcceptance` soundness) by
/// construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum BackendSendFailureClass {
    /// The send failed after or during an irrevocable backend write; the frame may or may not have
    /// been transmitted.
    Ambiguous,
    /// Configuration, pairing, or pre-dispatch invariant failure that cannot be retried.
    Fatal,
}

impl Error {
    /// Construct an [`Error::Backend`] with [`BackendSendFailureClass::Ambiguous`].
    pub fn backend_ambiguous(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Backend {
            class: BackendSendFailureClass::Ambiguous,
            source: Box::new(source),
        }
    }

    /// Construct an [`Error::Backend`] with [`BackendSendFailureClass::Fatal`].
    pub fn backend_fatal(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Backend {
            class: BackendSendFailureClass::Fatal,
            source: Box::new(source),
        }
    }

    /// Classify this transport error into its [`TransportSendFailureClass`], total over all
    /// compiled backend variants.
    pub const fn send_failure_class(&self) -> TransportSendFailureClass {
        match self {
            Self::SendPermitRevoked | Self::LinkCreditClosed(_) => {
                TransportSendFailureClass::PreAcceptance
            }
            Self::MessageNotDelivered(_) => TransportSendFailureClass::Ambiguous,
            Self::Backend {
                class: BackendSendFailureClass::Ambiguous,
                ..
            } => TransportSendFailureClass::Ambiguous,
            Self::Backend {
                class: BackendSendFailureClass::Fatal,
                ..
            } => TransportSendFailureClass::Fatal,
            #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
            Self::Webrtc(_)
            | Self::NativeSendCompletionTimeout { .. }
            | Self::NativeSendTask(_)
            | Self::NativeSendPanic(_) => TransportSendFailureClass::Ambiguous,
            #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
            Self::NativeSendRuntimeUnavailable
            | Self::NativeConnectionCloseTask(_)
            | Self::NativeConnectionRetirementTimeout { .. } => TransportSendFailureClass::Fatal,
            #[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
            Self::WebSysWebrtc(_) => TransportSendFailureClass::Ambiguous,
            #[cfg(feature = "dummy")]
            Self::DummyIrrevocableSendTaskStopped => TransportSendFailureClass::Ambiguous,
            #[cfg(feature = "dummy")]
            Self::DummyConnectionRetiredBeforeDispatch
            | Self::DummyRemoteConnectionUnavailable
            | Self::DummyRemoteConnectionClosed => TransportSendFailureClass::Fatal,
            Self::Codec(_)
            | Self::IceServer(_)
            | Self::ChannelOpen(_)
            | Self::Timer(_)
            | Self::ChannelMessage(_)
            | Self::SendByteCountOverflow
            | Self::FrameTooLarge { .. }
            | Self::LocalHandshakeGenerationError(_)
            | Self::RemoteHandshakeApplyError(_)
            | Self::UnsupportedOperation(_)
            | Self::RemoteMaxMessageSizeTooSmall { .. }
            | Self::UdpPortRange(_)
            | Self::ConnectionAlreadyExists(_)
            | Self::ConnectionNotFound(_)
            | Self::ConnectionReleased(_)
            | Self::RwLockWrite(_)
            | Self::RwLockRead(_)
            | Self::ForeignCredit(_)
            | Self::CreditPumpUnavailable(_)
            | Self::ChannelPoolEmpty => TransportSendFailureClass::Fatal,
        }
    }

    /// Whether an outbound send failure on a peer link is attributable to a broken or
    /// misbehaving remote link rather than pre-acceptance retirement, caller sizing, or local
    /// runtime shutdown. Total over all compiled backend variants.
    pub const fn blames_remote_peer_on_send(&self) -> bool {
        match self {
            Self::SendPermitRevoked | Self::LinkCreditClosed(_) => false,
            Self::MessageNotDelivered(_)
            | Self::ChannelOpen(_)
            | Self::ChannelMessage(_)
            | Self::RemoteHandshakeApplyError(_)
            | Self::RemoteMaxMessageSizeTooSmall { .. } => true,
            Self::Backend {
                class: BackendSendFailureClass::Ambiguous,
                ..
            } => true,
            Self::Backend {
                class: BackendSendFailureClass::Fatal,
                ..
            } => false,
            #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
            Self::Webrtc(_)
            | Self::NativeSendCompletionTimeout { .. }
            | Self::NativeSendTask(_)
            | Self::NativeSendPanic(_) => true,
            #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
            Self::NativeSendRuntimeUnavailable
            | Self::NativeConnectionCloseTask(_)
            | Self::NativeConnectionRetirementTimeout { .. } => false,
            #[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
            Self::WebSysWebrtc(_) => true,
            #[cfg(feature = "dummy")]
            Self::DummyRemoteConnectionUnavailable | Self::DummyRemoteConnectionClosed => true,
            #[cfg(feature = "dummy")]
            Self::DummyIrrevocableSendTaskStopped | Self::DummyConnectionRetiredBeforeDispatch => {
                false
            }
            Self::Codec(_)
            | Self::IceServer(_)
            | Self::Timer(_)
            | Self::SendByteCountOverflow
            | Self::FrameTooLarge { .. }
            | Self::LocalHandshakeGenerationError(_)
            | Self::UnsupportedOperation(_)
            | Self::UdpPortRange(_)
            | Self::ConnectionAlreadyExists(_)
            | Self::ConnectionNotFound(_)
            | Self::ConnectionReleased(_)
            | Self::RwLockWrite(_)
            | Self::RwLockRead(_)
            | Self::ForeignCredit(_)
            | Self::CreditPumpUnavailable(_)
            | Self::ChannelPoolEmpty => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Error;
    use super::IceServerError;
    use super::TransportSendFailureClass;

    /// Law (Pre-acceptance soundness, Lemma (T)): across the closed set of common and
    /// feature-enabled transport errors, only `SendPermitRevoked` and `LinkCreditClosed` classify
    /// as `PreAcceptance`; post-irrevocable delivery failures classify as `Ambiguous`, and
    /// configuration/invariant failures classify as `Fatal`.
    #[test]
    fn test_send_failure_class_is_total_and_preserves_pre_acceptance_soundness() {
        let pre_acceptance = [
            Error::SendPermitRevoked,
            Error::LinkCreditClosed("peer".to_string()),
        ];
        for err in &pre_acceptance {
            assert_eq!(
                err.send_failure_class(),
                TransportSendFailureClass::PreAcceptance,
                "expected PreAcceptance for {err:?}"
            );
        }

        let ambiguous = [
            Error::MessageNotDelivered("closed".to_string()),
            #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
            Error::NativeSendCompletionTimeout { timeout_ms: 100 },
            #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
            Error::NativeSendPanic("boom".to_string()),
            #[cfg(feature = "dummy")]
            Error::DummyIrrevocableSendTaskStopped,
        ];
        for err in &ambiguous {
            assert_eq!(
                err.send_failure_class(),
                TransportSendFailureClass::Ambiguous,
                "expected Ambiguous for {err:?}"
            );
        }

        let fatal = [
            Error::IceServer(IceServerError::UrlMissHost),
            Error::ChannelOpen("open".to_string()),
            Error::ChannelMessage("msg".to_string()),
            Error::SendByteCountOverflow,
            Error::LocalHandshakeGenerationError("sdp".to_string()),
            Error::RemoteMaxMessageSizeTooSmall {
                advertised: 100,
                minimum: 6123,
            },
            Error::UdpPortRange("range".to_string()),
            Error::ConnectionAlreadyExists("peer".to_string()),
            Error::ConnectionNotFound("peer".to_string()),
            Error::ConnectionReleased("peer".to_string()),
            Error::RwLockWrite("w".to_string()),
            Error::RwLockRead("r".to_string()),
            Error::ForeignCredit("peer".to_string()),
            Error::CreditPumpUnavailable("peer".to_string()),
            Error::ChannelPoolEmpty,
            #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
            Error::NativeSendRuntimeUnavailable,
            #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
            Error::NativeConnectionRetirementTimeout { timeout_ms: 100 },
            #[cfg(feature = "dummy")]
            Error::DummyConnectionRetiredBeforeDispatch,
            #[cfg(feature = "dummy")]
            Error::DummyRemoteConnectionUnavailable,
            #[cfg(feature = "dummy")]
            Error::DummyRemoteConnectionClosed,
        ];
        for err in &fatal {
            assert_eq!(
                err.send_failure_class(),
                TransportSendFailureClass::Fatal,
                "expected Fatal for {err:?}"
            );
        }
    }
}
