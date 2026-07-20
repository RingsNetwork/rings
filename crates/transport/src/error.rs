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

    #[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
    /// WebSysWebRTC error: {}
    #[error("WebSysWebRTC error: {}", dump_js_value(.0))]
    WebSysWebrtc(wasm_bindgen::JsValue),

    /// Bincode error: {0}
    #[error("Bincode error: {0}")]
    Bincode(#[from] bincode::Error),

    /// IceServer error: {0}
    #[error("IceServer error: {0}")]
    IceServer(#[from] IceServerError),

    /// Failed when waiting for data channel open: {0}
    #[error("Failed when waiting for data channel open: {0}")]
    DataChannelOpen(String),

    /// Message was not delivered: {0}
    #[error("Message was not delivered: {0}")]
    MessageNotDelivered(String),

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
