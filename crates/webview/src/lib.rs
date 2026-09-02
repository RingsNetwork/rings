#![deny(missing_docs)]
//! Rings webview gateway primitives.
//!
//! This crate owns the reusable, UI-independent pieces required to serve remote
//! pages through a Rings-owned gateway route: target URL encoding, typed gateway
//! requests and responses, header policy, virtual cookies, response rewriting,
//! and a pluggable transport boundary.
//!
//! A WebView host must apply [`GatewayRoutePolicy`] before opening a browser connection. That
//! trusted boundary redirects navigation, rejects cross-target runtime reads, and keeps direct
//! remote traffic out of the browser network stack.
//!
//! [`TargetUrl`] also rejects private, loopback, link-local, and obvious local-name targets before
//! a pluggable transport runs. Native transports remain responsible for resolving domains once,
//! applying the same public-address policy to the DNS snapshot, and connecting only to admitted
//! addresses so DNS rebinding cannot bypass this first gate.
//!
//! ## Opaque-origin deployment boundary
//!
//! Production navigation responses deliberately use CSP `sandbox` without
//! `allow-same-origin`. The resulting document has an opaque origin and is not a controlled
//! Service Worker client, so page-authored `fetch` and `XMLHttpRequest` cannot use the worker's
//! runtime gateway route. The production target set is therefore static and server-rendered
//! pages plus rewritten subresources. The `browser` bootstrap's dynamic-request adapters are
//! reusable only in a host that provides a separate trusted runtime-request bridge. Adding
//! `allow-same-origin` is not a compatibility fix: combined with `allow-scripts`, it would remove
//! the isolation boundary from same-origin gateway documents.

/// Virtual cookie jar for target-origin cookies.
pub mod cookie;
/// Virtual CORS request and response policy.
pub mod cors;
/// Error and result types.
pub mod error;
/// Response header normalization policy.
pub mod header;
/// HTML and CSS rewriting helpers.
pub mod rewrite;
/// Host request routing policy for controlled webview origins.
pub mod route;
/// Pluggable gateway transport and policy wrapper.
pub mod transport;
/// Typed gateway request and response DTOs.
pub mod types;
/// Target URL encoding and gateway route helpers.
pub mod url;

/// Browser runtime bootstrap helpers.
#[cfg(feature = "browser")]
pub mod browser;

pub use cookie::CookieJar;
pub use cors::preflight_request;
pub use cors::validate_response;
pub use error::CookieFailure;
pub use error::CorsFailure;
pub use error::GatewayFailure;
pub use error::GatewayFailureCode;
pub use error::Result;
pub use error::TransportFailure;
pub use error::WebviewError;
pub use header::HeaderPolicy;
pub use rewrite::RewriteContext;
pub use route::GatewayRoute;
pub use route::GatewayRoutePolicy;
pub use route::GatewayRouteRejection;
pub use transport::ConcurrentWebviewGateway;
pub use transport::GatewayResponseBodyLimit;
pub use transport::GatewayTransport;
pub use types::GatewayCredentials;
pub use types::GatewayHeader;
pub use types::GatewayRequest;
pub use types::GatewayRequestKind;
pub use types::GatewayResponse;
pub use url::GatewayPrefix;
pub use url::TargetUrl;
