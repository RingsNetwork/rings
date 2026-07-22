use thiserror::Error;

/// Result type used by the webview gateway.
pub type Result<T> = std::result::Result<T, WebviewError>;

/// Stable browser-facing failure metadata for one gateway request.
///
/// This is the typed boundary between a transport adapter and browser UI. The
/// gateway response can use `status`, `code`, and `summary` without parsing the
/// human-readable `detail` message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayFailure {
    status: u16,
    code: String,
    summary: String,
    detail: String,
}

impl GatewayFailure {
    /// Build gateway failure metadata.
    pub fn new(
        status: u16,
        code: impl Into<String>,
        summary: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status,
            code: code.into(),
            summary: summary.into(),
            detail: detail.into(),
        }
    }

    /// HTTP status to return from the controlled gateway route.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Stable machine-readable failure code.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Short user-facing failure summary.
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Detailed diagnostic message for the debug console.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for GatewayFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

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
    /// A transport adapter returned stable browser-facing failure metadata.
    #[error("{0}")]
    GatewayFailure(GatewayFailure),
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
