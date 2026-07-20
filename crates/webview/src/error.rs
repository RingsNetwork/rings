use thiserror::Error;

/// Result type used by the webview gateway.
pub type Result<T> = std::result::Result<T, WebviewError>;

/// Errors raised while normalizing, rewriting, or forwarding gateway traffic.
#[derive(Debug, Error)]
pub enum WebviewError {
    /// A gateway prefix is not a path prefix owned by the local application.
    #[error("invalid gateway prefix {0:?}")]
    InvalidGatewayPrefix(String),
    /// A gateway URL did not contain an encoded target URL.
    #[error("invalid gateway URL {0:?}")]
    InvalidGatewayUrl(String),
    /// Percent-decoding failed.
    #[error("failed to decode gateway URL {0:?}")]
    Decode(String),
    /// Controlled-origin configuration is invalid.
    #[error("invalid controlled origin {0:?}")]
    InvalidControlledOrigin(String),
    /// The target URL could not be parsed.
    #[error("invalid target URL: {0}")]
    Url(#[from] url::ParseError),
    /// The target scheme is outside the gateway policy.
    #[error("unsupported target URL scheme {0:?}")]
    UnsupportedScheme(String),
    /// Header validation or policy normalization failed.
    #[error("header policy error: {0}")]
    Header(String),
    /// Cookie parsing or matching failed.
    #[error("cookie policy error: {0}")]
    Cookie(String),
    /// A cross-origin runtime response did not satisfy the virtual CORS policy.
    #[error("CORS policy error: {0}")]
    Cors(String),
    /// The pluggable transport failed.
    #[error("gateway transport failed: {0}")]
    Transport(String),
    /// A gateway response could not be rendered as a page.
    #[error("webview render failed: {0}")]
    Render(String),
    /// Browser integration failed.
    #[cfg(feature = "browser")]
    #[error("browser integration failed: {0}")]
    Browser(String),
}
