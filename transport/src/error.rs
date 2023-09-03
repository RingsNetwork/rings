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

    #[error("Bincode error: {0}")]
    Bincode(#[from] bincode::Error),

    #[error("IceServer error: {0}")]
    IceServer(#[from] IceServerError),

    #[error("Failed when waiting for data channel open: {0}")]
    DataChannelOpen(String),

    #[error("WebRTC local SDP generation error")]
    WebrtcLocalSdpGenerationError,

    #[error("Connection {0} already exists")]
    ConnectionAlreadyExists(String),

    #[error("Connection {0} not found, should handshake first")]
    ConnectionNotFound(String),
}
