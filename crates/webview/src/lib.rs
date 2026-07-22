#![deny(missing_docs)]
//! Rings webview gateway primitives.
//!
//! This crate owns the reusable, UI-independent pieces required to serve remote
//! pages through a Rings-owned gateway route: target URL encoding, typed gateway
//! requests and responses, header policy, virtual cookies, response rewriting,
//! and a pluggable transport boundary.
//!
//! `browser` bootstrap hooks preserve ordinary page behavior, but a WebView host
//! must apply [`GatewayRoutePolicy`] before opening a browser connection. That
//! trusted boundary redirects navigation, rejects cross-target runtime reads,
//! and keeps direct remote traffic out of the browser network stack.

/// Virtual cookie jar for target-origin cookies.
pub mod cookie;
/// Virtual CORS request and response policy.
pub mod cors;
/// Error and result types.
pub mod error;
/// Response header normalization policy.
pub mod header;
/// Render target pages into gateway-rewritten HTML.
pub mod render;
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
pub use error::GatewayFailure;
pub use error::Result;
pub use error::WebviewError;
pub use header::HeaderPolicy;
pub use render::RenderedPage;
pub use render::WebviewRenderer;
pub use rewrite::RewriteContext;
pub use route::GatewayRoute;
pub use route::GatewayRoutePolicy;
pub use route::GatewayRouteRejection;
pub use transport::ConcurrentWebviewGateway;
pub use transport::GatewayTransport;
pub use transport::WebviewGateway;
pub use types::GatewayCredentials;
pub use types::GatewayHeader;
pub use types::GatewayRequest;
pub use types::GatewayRequestKind;
pub use types::GatewayResponse;
pub use url::GatewayPrefix;
pub use url::TargetUrl;
