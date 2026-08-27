use thiserror::Error;

/// Result type used by the webview gateway.
pub type Result<T> = std::result::Result<T, WebviewError>;

/// Closed causes for a virtual CORS policy rejection.
///
/// The policy does not retain upstream headers or origins in its domain error:
/// they are untrusted input and are only useful when an adapter renders a
/// diagnostic.  Callers can instead branch on the policy fact directly.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CorsFailure {
    /// A cross-origin runtime request did not identify its controlled source.
    #[error("cross-origin request has no trusted source origin")]
    MissingTrustedSourceOrigin,
    /// The upstream response did not allow the controlled source origin.
    #[error("response does not allow the controlled source origin")]
    OriginNotAllowed,
    /// A credentialed response omitted `Access-Control-Allow-Credentials: true`.
    #[error("credentialed response lacks Access-Control-Allow-Credentials: true")]
    CredentialsNotAllowed,
    /// A preflight response used a non-success HTTP status.
    #[error("preflight returned HTTP {status}")]
    PreflightStatus {
        /// HTTP status returned by the preflight response.
        status: u16,
    },
    /// A preflight response omitted the requested method.
    #[error("preflight does not allow the requested method")]
    PreflightMethodNotAllowed,
    /// A preflight response omitted one or more requested headers.
    #[error("preflight does not allow every requested header")]
    PreflightHeadersNotAllowed,
    /// A credentialed preflight omitted `Access-Control-Allow-Credentials: true`.
    #[error("credentialed preflight lacks Access-Control-Allow-Credentials: true")]
    PreflightCredentialsNotAllowed,
}

/// Closed causes raised by the virtual cookie jar.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CookieFailure {
    /// The cookie origin has no host component.
    #[error("cookie origin host is empty")]
    MissingOriginHost,
    /// The upstream emitted an empty `Set-Cookie` value.
    #[error("empty Set-Cookie")]
    EmptySetCookie,
    /// The leading `name=value` pair in `Set-Cookie` was malformed.
    #[error("invalid Set-Cookie pair")]
    InvalidNameValue,
    /// The leading `Set-Cookie` name was empty.
    #[error("cookie name is empty")]
    EmptyName,
    /// The shared virtual cookie jar lock was poisoned.
    #[error("virtual cookie jar lock is poisoned")]
    JarLockPoisoned,
}

/// Closed causes internal to request-session transport orchestration.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TransportFailure {
    /// This target has no ambient clock for the convenience gateway API.
    #[error("gateway clock is unavailable on this target")]
    ClockUnavailable,
    /// A prepared request session was sent more than once.
    #[error("gateway request session was sent twice")]
    RequestSessionSentTwice,
    /// A caller request exceeds the bounded gateway body budget.
    #[error("gateway request body has {actual} bytes, exceeding the {limit}-byte limit")]
    RequestBodyTooLarge {
        /// Body length supplied by the caller.
        actual: usize,
        /// Maximum request body length accepted by the gateway.
        limit: usize,
    },
    /// A transport response exceeds the bounded gateway body budget.
    #[error("gateway response body has {actual} bytes, exceeding the {limit}-byte limit")]
    ResponseBodyTooLarge {
        /// Body length returned by the transport.
        actual: usize,
        /// Maximum body length accepted by the gateway.
        limit: usize,
    },
    /// A concrete transport adapter failed after policy admission.
    #[error("{detail}")]
    Adapter {
        /// Adapter diagnostic rendered only at an error-reporting boundary.
        detail: String,
    },
}

impl TransportFailure {
    /// Wrap an adapter diagnostic in the generic closed transport failure class.
    pub fn adapter(detail: impl Into<String>) -> Self {
        Self::Adapter {
            detail: detail.into(),
        }
    }
}

/// Closed set of browser-facing gateway failure classes.
///
/// Each variant owns the public HTTP projection and short summary.  Rendering
/// adapters may turn the variant into a string code, but cannot publish an
/// unrecognised code or a mismatched status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayFailureCode {
    /// The upstream gateway transport failed.
    GatewayTransportFailed,
    /// The local gateway's bounded admission queue is full.
    GatewayOverloaded,
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
            Self::GatewayOverloaded => "gateway_overloaded",
            Self::OnionExitUnavailable => "onion_exit_unavailable",
            Self::OnionRouteUnavailable => "onion_route_unavailable",
            Self::OnionRequestTimedOut => "onion_request_timed_out",
        }
    }

    /// HTTP status associated with this failure class.
    pub const fn status(self) -> u16 {
        match self {
            Self::GatewayTransportFailed => 502,
            Self::GatewayOverloaded => 503,
            Self::OnionExitUnavailable | Self::OnionRouteUnavailable => 503,
            Self::OnionRequestTimedOut => 504,
        }
    }

    /// Short user-facing failure summary.
    pub const fn summary(self) -> &'static str {
        match self {
            Self::GatewayTransportFailed => "Gateway transport failed.",
            Self::GatewayOverloaded => "The local gateway is overloaded.",
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
    Cookie(#[from] CookieFailure),
    /// A cross-origin runtime response did not satisfy the virtual CORS policy.
    #[error("CORS policy error: {0}")]
    Cors(#[from] CorsFailure),
    /// A runtime fetch or XHR was built without trusted source context.
    #[error("runtime gateway request requires a trusted source origin")]
    MissingRuntimeSourceOrigin,
    /// A transport adapter returned stable browser-facing failure metadata.
    #[error("{0}")]
    GatewayFailure(GatewayFailure),
    /// The pluggable transport failed.
    #[error("gateway transport failed: {0}")]
    Transport(#[from] TransportFailure),
    /// A gateway response could not be rendered as a page.
    #[error("webview render failed: {0}")]
    Render(String),
    /// An HTML or CSS response cannot be safely rewritten because it is not UTF-8.
    #[error("cannot safely rewrite {content_type:?} response as UTF-8")]
    UnrewritableTextEncoding {
        /// Upstream response media type that requires rewriting.
        content_type: String,
    },
    /// Nested `srcdoc` rewriting reached the explicit recursion bound.
    #[error("nested srcdoc depth exceeds limit {max_depth}")]
    SrcdocNestingLimit {
        /// Maximum nested `srcdoc` documents accepted by the rewriter.
        max_depth: u8,
    },
    /// A top-level response has no media type that the gateway can safely rewrite or pass through.
    #[error("unsafe navigation response media type {content_type:?}")]
    UnsafeNavigationMediaType {
        /// Normalized upstream media type, or `None` when the header was absent.
        content_type: Option<String>,
    },
    /// Browser integration failed.
    #[cfg(feature = "browser")]
    #[error("browser integration failed: {0}")]
    Browser(String),
}

impl WebviewError {
    /// Build a generic typed transport failure from an adapter diagnostic.
    pub fn transport(detail: impl Into<String>) -> Self {
        TransportFailure::adapter(detail).into()
    }
}

#[cfg(test)]
mod test_error;
