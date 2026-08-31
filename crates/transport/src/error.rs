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
}

/// Errors produced by transport connections, pools, and backend adapters.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// IO error: {0}
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[cfg(feature = "native-webrtc")]
    /// WebRTC error: {0}
    #[error("WebRTC error: {0}")]
    Webrtc(#[from] webrtc::error::Error),

    #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
    /// The host failed to enable or apply its direct-underlay authorization policy.
    #[error(transparent)]
    UnderlayCandidateAdmission(#[from] crate::connections::UnderlayCandidateAdmissionError),

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

    /// Failed when waiting for data channel open: {0}
    #[error("Failed when waiting for data channel open: {0}")]
    DataChannelOpen(String),

    /// Message was not delivered: {0}
    #[error("Message was not delivered: {0}")]
    MessageNotDelivered(String),

    /// A data-channel message used an unsupported representation: {0}
    #[error("Unsupported data-channel message: {0}")]
    DataChannelMessage(String),

    /// The higher-level authorization was revoked before backend send admission.
    #[error("Send permit was revoked before transport send admission")]
    SendPermitRevoked,

    #[cfg(feature = "native-webrtc")]
    /// No Tokio runtime is available to drive a native data-channel write.
    #[error("Native data-channel send requires an active Tokio runtime")]
    NativeSendRuntimeUnavailable,

    #[cfg(feature = "native-webrtc")]
    /// An irrevocable native data-channel write exceeded its completion bound.
    #[error("Native data-channel send did not complete within {timeout_ms}ms")]
    NativeSendCompletionTimeout {
        /// Completion bound applied after the write became irrevocable.
        timeout_ms: u128,
    },

    #[cfg(feature = "native-webrtc")]
    /// A native task driving an irrevocable data-channel write stopped unexpectedly.
    #[error("Native data-channel send task stopped: {0}")]
    NativeSendTask(#[source] tokio::task::JoinError),

    #[cfg(feature = "native-webrtc")]
    /// A native data-channel send panicked while still owned by its caller.
    #[error("Native data-channel send panicked: {0}")]
    NativeSendPanic(String),

    #[cfg(feature = "native-webrtc")]
    /// A native task driving physical connection close stopped unexpectedly.
    #[error("Native connection close task stopped: {0}")]
    NativeConnectionCloseTask(#[source] tokio::task::JoinError),

    #[cfg(feature = "native-webrtc")]
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

    /// WebRTC local SDP generation error: {0}
    #[error("WebRTC local SDP generation error: {0}")]
    WebrtcLocalSdpGenerationError(String),

    /// WebRTC UDP port range was rejected by the ICE stack: {0}
    #[error("WebRTC UDP port range was rejected by the ICE stack: {0}")]
    WebrtcUdpPortRange(String),

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

    /// Cannot select from an empty round-robin pool
    #[error("Cannot select from an empty round-robin pool")]
    RoundRobinPoolEmpty,
}

#[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
fn dump_js_value(v: &wasm_bindgen::JsValue) -> String {
    let Ok(s) = js_sys::JSON::stringify(v) else {
        return "Failed to stringify Error(JsValue)".to_string();
    };
    s.into()
}
