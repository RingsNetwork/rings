use thiserror::Error;

/// Result type used by the webview gateway.
pub type Result<T> = std::result::Result<T, WebviewError>;

/// Closed set of browser-facing gateway failure classes.
///
/// Each variant owns the public HTTP projection and short summary.  Rendering
/// adapters may turn the variant into a string code, but cannot publish an
/// unrecognised code or a mismatched status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayFailureCode {
    /// The upstream gateway transport failed.
    GatewayTransportFailed,
    /// No live onion HTTPS exit is available.
    OnionExitUnavailable,
    /// No onion route exists for the requested target.
    OnionRouteUnavailable,
    /// The onion HTTPS request timed out.
    OnionRequestTimedOut,
}

impl GatewayFailureCode {
    /// Stable browser-visible error code.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GatewayTransportFailed => "gateway_transport_failed",
            Self::OnionExitUnavailable => "onion_exit_unavailable",
            Self::OnionRouteUnavailable => "onion_route_unavailable",
            Self::OnionRequestTimedOut => "onion_request_timed_out",
        }
    }

    /// HTTP status associated with this failure class.
    pub const fn status(self) -> u16 {
        match self {
            Self::GatewayTransportFailed => 502,
            Self::OnionExitUnavailable | Self::OnionRouteUnavailable => 503,
            Self::OnionRequestTimedOut => 504,
        }
    }

    /// Short user-facing failure summary.
    pub const fn summary(self) -> &'static str {
        match self {
            Self::GatewayTransportFailed => "Gateway transport failed.",
            Self::OnionExitUnavailable => "No live HTTPS onion exit is available.",
            Self::OnionRouteUnavailable => {
                "No onion route is currently available for the requested target."
            }
            Self::OnionRequestTimedOut => "Onion HTTPS proxy request timed out.",
        }
    }
}

/// Stable browser-facing failure metadata for one gateway request.
///
/// This is the typed boundary between a transport adapter and browser UI. The
/// gateway response can use `status`, `code`, and `summary` without parsing the
/// human-readable `detail` message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayFailure {
    code: GatewayFailureCode,
    detail: String,
}

impl GatewayFailure {
    /// Build gateway failure metadata from a closed public failure class.
    pub fn new(code: GatewayFailureCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// HTTP status to return from the controlled gateway route.
    pub fn status(&self) -> u16 {
        self.code.status()
    }

    /// Stable machine-readable failure code.
    pub fn code(&self) -> &str {
        self.code.as_str()
    }

    /// Short user-facing failure summary.
    pub fn summary(&self) -> &str {
        self.code.summary()
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

#[cfg(test)]
mod tests {
    use super::GatewayFailureCode;

    #[test]
    fn gateway_failure_codes_have_exhaustive_http_ui_projections() {
        let projections = [
            (
                GatewayFailureCode::GatewayTransportFailed,
                502,
                "gateway_transport_failed",
                "Gateway transport failed.",
            ),
            (
                GatewayFailureCode::OnionExitUnavailable,
                503,
                "onion_exit_unavailable",
                "No live HTTPS onion exit is available.",
            ),
            (
                GatewayFailureCode::OnionRouteUnavailable,
                503,
                "onion_route_unavailable",
                "No onion route is currently available for the requested target.",
            ),
            (
                GatewayFailureCode::OnionRequestTimedOut,
                504,
                "onion_request_timed_out",
                "Onion HTTPS proxy request timed out.",
            ),
        ];
        for (code, status, name, summary) in projections {
            assert_eq!(code.status(), status);
            assert_eq!(code.as_str(), name);
            assert_eq!(code.summary(), summary);
        }
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
    /// A runtime fetch or XHR was built without trusted source context.
    #[error("runtime gateway request requires a trusted source origin")]
    MissingRuntimeSourceOrigin,
    /// A transport adapter returned stable browser-facing failure metadata.
    #[error("{0}")]
    GatewayFailure(GatewayFailure),
    /// The pluggable transport failed.
    #[error("gateway transport failed: {0}")]
    Transport(String),
    /// A gateway response could not be rendered as a page.
    #[error("webview render failed: {0}")]
    Render(String),
    /// An HTML or CSS response cannot be safely rewritten because it is not UTF-8.
    #[error("cannot safely rewrite {content_type:?} response as UTF-8")]
    UnrewritableTextEncoding {
        /// Upstream response media type that requires rewriting.
        content_type: String,
    },
    /// Browser integration failed.
    #[cfg(feature = "browser")]
    #[error("browser integration failed: {0}")]
    Browser(String),
}
