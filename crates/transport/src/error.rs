#![allow(missing_docs)]

pub type Result<T> = std::result::Result<T, Error>;

#[derive(thiserror::Error, Debug)]
pub enum IceServerError {
    #[error("Url parse error")]
    UrlParse(#[from] url::ParseError),

    #[error("Ice server scheme {0} has not supported yet")]
    SchemeNotSupported(String),

    #[error("Cannot extract host from url")]
    UrlMissHost,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[cfg(feature = "native-webrtc")]
    #[error("WebRTC error: {0}")]
    Webrtc(#[from] webrtc::error::Error),

    #[cfg(feature = "web-sys-webrtc")]
    #[error("WebSysWebRTC error: {}", dump_js_value(.0))]
    WebSysWebrtc(wasm_bindgen::JsValue),

    #[error("Bincode error: {0}")]
    Bincode(#[from] bincode::Error),

    #[error("IceServer error: {0}")]
    IceServer(#[from] IceServerError),

    #[error("Failed when waiting for data channel open: {0}")]
    DataChannelOpen(String),

    #[error("Message was not delivered: {0}")]
    MessageNotDelivered(String),

    #[error("WebRTC local SDP generation error: {0}")]
    WebrtcLocalSdpGenerationError(String),

    #[error("remote SDP rejected before apply: {0}")]
    RemoteSdpRejected(String),

    #[error("Connection {0} already exists")]
    ConnectionAlreadyExists(String),

    #[error("Connection {0} not found, should handshake first")]
    ConnectionNotFound(String),

    #[error("Connection {0} is released")]
    ConnectionReleased(String),

    #[error("Rwlock try write failed: {0}")]
    RwLockWrite(String),

    #[error("Rwlock try read failed: {0}")]
    RwLockRead(String),
}

impl Error {
    /// Whether an offer/answer call (`webrtc_answer_offer` / `webrtc_accept_answer` /
    /// `webrtc_create_offer`) failed *before* it mutated the `PeerConnection`'s signaling state —
    /// i.e. the remote SDP was rejected at parse/validation, never reaching `setRemoteDescription`/
    /// `setLocalDescription`. The connection is still consistent in that case, so a caller doing
    /// renegotiation can leave it intact rather than resetting it. The boundary is reported here, by
    /// the backend that actually knows where its `set*Description` call sits, instead of being guessed
    /// by an upper layer. Only meaningful for errors returned by those three methods.
    pub fn is_signaling_pre_apply(&self) -> bool {
        matches!(self, Error::RemoteSdpRejected(_))
    }
}

#[cfg(feature = "web-sys-webrtc")]
fn dump_js_value(v: &wasm_bindgen::JsValue) -> String {
    let Ok(s) = js_sys::JSON::stringify(v) else {
        return "Failed to stringify Error(JsValue)".to_string();
    };
    s.into()
}
