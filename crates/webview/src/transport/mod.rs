use std::sync::Mutex;
#[cfg(not(target_family = "wasm"))]
use std::time::SystemTime;
#[cfg(not(target_family = "wasm"))]
use std::time::UNIX_EPOCH;

use async_trait::async_trait;

use crate::cookie::CookieJar;
use crate::cors;
use crate::error::Result;
use crate::error::WebviewError;
use crate::header::HeaderPolicy;
use crate::rewrite::RewriteContext;
use crate::types::GatewayHeader;
use crate::types::GatewayRequest;
use crate::types::GatewayRequestKind;
use crate::types::GatewayResponse;
use crate::url::GatewayPrefix;
use crate::url::TargetUrl;

/// Default maximum request and response body size accepted by the gateway (8 MiB).
const MAX_GATEWAY_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Maximum response body a gateway transport may retain for one request.
///
/// The value is passed into the transport boundary so streaming adapters can stop reading before
/// allocating an oversized body. The gateway validates the returned value again as a defense
/// against adapters that violate the contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GatewayResponseBodyLimit(usize);

impl GatewayResponseBodyLimit {
    /// Standard body budget enforced by the webview gateway.
    pub const DEFAULT: Self = Self(MAX_GATEWAY_BODY_BYTES);

    /// Return the maximum retained body bytes.
    pub const fn bytes(self) -> usize {
        self.0
    }
}

/// Target-dependent transport ownership bound.
///
/// Native transports are safe to move between executor threads; browser transports stay on the
/// single-threaded wasm executor.
#[doc(hidden)]
#[cfg(not(target_family = "wasm"))]
pub trait GatewayTransportMaybeSend: Send + Sync {}
#[cfg(not(target_family = "wasm"))]
impl<T: Send + Sync> GatewayTransportMaybeSend for T {}
/// Target-dependent transport ownership bound.
#[doc(hidden)]
#[cfg(target_family = "wasm")]
pub trait GatewayTransportMaybeSend {}
#[cfg(target_family = "wasm")]
impl<T> GatewayTransportMaybeSend for T {}

/// Pluggable transport for normalized webview gateway requests.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait GatewayTransport: GatewayTransportMaybeSend {
    /// Send `request` through the concrete transport and return a raw response.
    ///
    /// Implementations must stop reading before retaining more than `body_limit` bytes. Returning
    /// a larger owned body is a contract violation and is rejected by the gateway as a fallback.
    async fn send(
        &self,
        request: GatewayRequest,
        body_limit: GatewayResponseBodyLimit,
    ) -> Result<GatewayResponse>;
}

/// A gateway that permits concurrent transport requests while sharing one virtual cookie jar.
///
/// The cookie jar is locked only while a request is prepared and while upstream `Set-Cookie`
/// headers are committed. Network I/O and response rewriting run outside that lock, so one slow
/// upstream request cannot block unrelated page resources.
pub struct ConcurrentWebviewGateway<T> {
    /// Immutable rewriting and header normalization policy shared across requests.
    policy: GatewayResponsePolicy,
    /// Underlying transport used to issue normalized upstream requests.
    transport: T,
    /// Partitioned virtual cookie jar synchronized across concurrent requests.
    cookies: Mutex<CookieJar>,
}

/// Shared request/response normalization, rewriting, and bootstrap configuration.
struct GatewayResponsePolicy {
    /// Gateway route prefix used for URL rewriting.
    prefix: GatewayPrefix,
    /// Request/response header normalization policy.
    header_policy: HeaderPolicy,
    /// Optional runtime bootstrap script injected into HTML/XHTML documents.
    bootstrap_script: Option<BootstrapScript>,
}

/// One request after its cookie-dependent preparation and before its cookie-dependent commit.
///
/// The session keeps the two synchronization points explicit: a caller prepares it while it can
/// read the virtual jar, then commits response cookies after transport I/O. Everything between
/// those points is independent of jar ownership and is therefore shared by both gateway
/// adapters.
struct GatewayRequestSession {
    /// Privacy-normalized request retained for CORS validation and cookie scoping.
    cors_request: GatewayRequest,
    /// Pending upstream request consumed once when sent.
    request: Option<GatewayRequest>,
    /// Optional CORS preflight request sent before the main request.
    preflight: Option<GatewayRequest>,
    /// Whether upstream `Set-Cookie` headers should be committed to the jar.
    stores_cookies: bool,
}

impl GatewayRequestSession {
    /// Validate and prepare a request session while holding read access to `cookies`.
    fn prepare(
        policy: &GatewayResponsePolicy,
        cookies: &CookieJar,
        request: GatewayRequest,
        now: i64,
    ) -> Result<Self> {
        validate_request_body_size(&request)?;
        let request = policy.prepare_request(cookies, request, now)?;
        // CORS is evaluated against the privacy-normalized request actually sent upstream. Thus
        // a dropped fingerprint header neither triggers a phantom preflight nor appears in the
        // preflight's requested-header set.
        let cors_request = request.clone();
        let preflight = cors::preflight_request(&request)?;
        let stores_cookies = request.allows_target_cookies();

        Ok(Self {
            cors_request,
            request: Some(request),
            preflight,
            stores_cookies,
        })
    }

    /// Execute any required CORS preflight and forward the main request through `transport`.
    async fn send<T>(
        &mut self,
        policy: &GatewayResponsePolicy,
        transport: &T,
    ) -> Result<GatewayResponse>
    where
        T: GatewayTransport,
    {
        if let Some(preflight) = self.preflight.take() {
            let preflight = policy.header_policy.normalize_request(preflight);
            let response = transport
                .send(preflight, GatewayResponseBodyLimit::DEFAULT)
                .await?;
            validate_response_body_size(&response, GatewayResponseBodyLimit::DEFAULT)?;
            cors::validate_preflight_response(&self.cors_request, &response)?;
        }
        let request = self
            .request
            .take()
            .ok_or(crate::error::TransportFailure::RequestSessionSentTwice)?;
        let response = transport
            .send(request, GatewayResponseBodyLimit::DEFAULT)
            .await?;
        validate_response_body_size(&response, GatewayResponseBodyLimit::DEFAULT)?;
        cors::validate_response(&self.cors_request, &response)?;
        Ok(response)
    }

    /// Persist upstream `Set-Cookie` headers into `cookies` when the request permits cookies.
    fn commit_response_cookies(
        &self,
        policy: &GatewayResponsePolicy,
        cookies: &mut CookieJar,
        response: &GatewayResponse,
        now: i64,
    ) -> Result<()> {
        if self.stores_cookies {
            policy.store_response_cookies(cookies, &self.cors_request, response, now)?;
        }
        Ok(())
    }

    /// Normalize headers, rewrite active document bodies, and recheck the post-rewrite body limit.
    fn finish(
        &self,
        policy: &GatewayResponsePolicy,
        response: GatewayResponse,
    ) -> Result<GatewayResponse> {
        let response = policy.finish_response(&self.cors_request, response)?;
        // Rewriting expands URLs and may inject a bootstrap script. The post-send limit alone
        // cannot bound this new body; retain a separate check before delivery to the browser.
        validate_response_body_size(&response, GatewayResponseBodyLimit::DEFAULT)?;
        Ok(response)
    }
}

/// Strategy for supplying the injected browser runtime bootstrap script.
enum BootstrapScript {
    /// Fixed script string injected into every rewritten HTML/XHTML document.
    Static(String),
    /// Callback computing a request-specific bootstrap script.
    PerRequest(fn(&GatewayRequest) -> String),
}

impl GatewayResponsePolicy {
    /// Build a default response policy for `prefix` without a bootstrap script.
    fn new(prefix: GatewayPrefix) -> Self {
        Self {
            header_policy: HeaderPolicy::new(prefix.clone()),
            prefix,
            bootstrap_script: None,
        }
    }

    /// Normalize request headers and attach matching virtual cookies at timestamp `now`.
    fn prepare_request(
        &self,
        cookies: &CookieJar,
        request: GatewayRequest,
        now: i64,
    ) -> Result<GatewayRequest> {
        if request.lacks_runtime_source_origin() {
            return Err(WebviewError::MissingRuntimeSourceOrigin);
        }
        let mut request = self.header_policy.normalize_request(request);
        if request.allows_target_cookies() {
            if let Some(cookie_header) = cookies.cookie_header_for_request_at(&request, now) {
                request
                    .headers
                    .push(GatewayHeader::new("Cookie", cookie_header)?);
            }
        }
        Ok(request)
    }

    /// Store every `Set-Cookie` header from `response` into `cookies`.
    fn store_response_cookies(
        &self,
        cookies: &mut CookieJar,
        request: &GatewayRequest,
        response: &GatewayResponse,
        now: i64,
    ) -> Result<()> {
        for header in response
            .headers
            .iter()
            .filter(|header| header.name_eq("set-cookie"))
        {
            cookies.store_set_cookie_for_request_at(request, header.value.as_str(), now)?;
        }
        Ok(())
    }

    /// Normalize response headers, rewrite the body when required, and filter CORS-exposed headers.
    fn finish_response(
        &self,
        request: &GatewayRequest,
        response: GatewayResponse,
    ) -> Result<GatewayResponse> {
        let mut response = self
            .header_policy
            .normalize_response(&request.target, response)?;
        response.body = self.rewrite_body(request, &response)?;
        Ok(cors::filter_exposed_response_headers(request, response))
    }

    /// Rewrite HTML, XHTML, SVG, or CSS response bodies and reject unsafe navigation media types.
    fn rewrite_body(
        &self,
        request: &GatewayRequest,
        response: &GatewayResponse,
    ) -> Result<Vec<u8>> {
        let content_type = response
            .headers
            .iter()
            .find(|header| header.name_eq("content-type"))
            .map(|header| normalize_media_type(header.value.as_str()));
        let rewrite_kind =
            rewrite_kind_for_response(content_type.as_deref(), response.body.as_slice());
        let Some(rewrite_kind) = rewrite_kind else {
            if request.kind == GatewayRequestKind::Navigation
                && !content_type
                    .as_deref()
                    .is_some_and(is_inert_navigation_media_type)
            {
                return Err(WebviewError::UnsafeNavigationMediaType { content_type });
            }
            return Ok(response.body.clone());
        };
        let content_type = content_type.unwrap_or_else(|| "sniffed active document".to_string());
        let text = std::str::from_utf8(response.body.as_slice()).map_err(|_| {
            // Rewriting is the boundary that keeps navigable text on the controlled origin.
            // Passing an undecodable document through would bypass that boundary.
            WebviewError::UnrewritableTextEncoding { content_type }
        })?;
        let mut ctx = RewriteContext::new(self.prefix.clone(), request.target.clone());
        if let Some(script) = self.bootstrap_for(request) {
            match rewrite_kind {
                RewriteKind::Html => ctx = ctx.with_bootstrap_script(script),
                RewriteKind::Xhtml => {
                    ctx = ctx.with_xhtml_bootstrap_script(script);
                }
                // The HTML bootstrap assumes a Document/HTMLElement surface. SVG navigation is
                // still statically rewritten and sandboxed, but receives no incompatible HTML
                // runtime injection.
                RewriteKind::Svg | RewriteKind::Css => {}
            }
        }
        let rewritten = match rewrite_kind {
            RewriteKind::Html | RewriteKind::Xhtml => {
                ctx.rewrite_html_with_limit(text, MAX_GATEWAY_BODY_BYTES)?
            }
            RewriteKind::Svg => ctx.rewrite_svg_with_limit(text, MAX_GATEWAY_BODY_BYTES)?,
            RewriteKind::Css => ctx.rewrite_css_with_limit(text, MAX_GATEWAY_BODY_BYTES)?,
        };
        Ok(rewritten.into_bytes())
    }

    /// Resolve the configured bootstrap script for `request`, if any.
    fn bootstrap_for(&self, request: &GatewayRequest) -> Option<String> {
        match &self.bootstrap_script {
            Some(BootstrapScript::Static(script)) => Some(script.clone()),
            Some(BootstrapScript::PerRequest(build)) => Some(build(request)),
            None => None,
        }
    }
}

/// Active document format selected for response-body rewriting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RewriteKind {
    /// Standard HTML document.
    Html,
    /// XHTML document served as `application/xhtml+xml`.
    Xhtml,
    /// SVG document served as `image/svg+xml`.
    Svg,
    /// CSS stylesheet served as `text/css`.
    Css,
}

/// Verify that `response.body` does not exceed `body_limit`.
fn validate_response_body_size(
    response: &GatewayResponse,
    body_limit: GatewayResponseBodyLimit,
) -> Result<()> {
    if response.body.len() > body_limit.bytes() {
        return Err(crate::error::TransportFailure::ResponseBodyTooLarge {
            actual: response.body.len(),
            limit: body_limit.bytes(),
        }
        .into());
    }
    Ok(())
}

/// Verify that `request.body` does not exceed `MAX_GATEWAY_BODY_BYTES`.
fn validate_request_body_size(request: &GatewayRequest) -> Result<()> {
    if request.body.len() > MAX_GATEWAY_BODY_BYTES {
        return Err(crate::error::TransportFailure::RequestBodyTooLarge {
            actual: request.body.len(),
            limit: MAX_GATEWAY_BODY_BYTES,
        }
        .into());
    }
    Ok(())
}

/// Extract the trimmed, lowercased MIME essence from a `Content-Type` header value.
fn normalize_media_type(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Map a normalized MIME type to its corresponding rewriter mode, if active.
fn rewrite_kind_for_media_type(media_type: &str) -> Option<RewriteKind> {
    match media_type {
        "text/html" => Some(RewriteKind::Html),
        "application/xhtml+xml" => Some(RewriteKind::Xhtml),
        "image/svg+xml" => Some(RewriteKind::Svg),
        "text/css" => Some(RewriteKind::Css),
        _ => None,
    }
}

/// Classify a body at the declared-media boundary.
///
/// `HeaderPolicy` emits `X-Content-Type-Options: nosniff`, so an explicit media type is
/// authoritative: declared inert data must never become active merely because its bytes begin
/// with `<`. Sniffing is reserved for an absent declaration, where rewriting is the only safe
/// way to admit a body that proves an active-document prefix.
fn rewrite_kind_for_response(content_type: Option<&str>, body: &[u8]) -> Option<RewriteKind> {
    match content_type {
        Some(media_type) => rewrite_kind_for_media_type(media_type),
        None => sniff_active_document(body),
    }
}

/// Return whether `media_type` is an inert non-active format safe for navigation passthrough.
fn is_inert_navigation_media_type(media_type: &str) -> bool {
    media_type == "text/plain"
        || media_type == "application/json"
        || media_type.ends_with("+json")
        || matches!(
            media_type,
            "application/pdf"
                | "image/avif"
                | "image/bmp"
                | "image/gif"
                | "image/jpeg"
                | "image/png"
                | "image/webp"
                | "image/x-icon"
                | "audio/mpeg"
                | "audio/ogg"
                | "video/mp4"
                | "video/ogg"
                | "video/webm"
        )
}

/// Sniff the leading bytes of an untyped response body for markup that requires HTML rewriting.
fn sniff_active_document(body: &[u8]) -> Option<RewriteKind> {
    const SNIFF_PREFIX_BYTES: usize = 4096;
    let prefix_len = body.len().min(SNIFF_PREFIX_BYTES);
    let prefix_bytes = body.get(..prefix_len).unwrap_or(body);
    // HTML's active marker is ASCII-compatible. Lossy decoding of a bounded prefix avoids
    // treating an unrelated invalid byte later in the body as evidence that the prefix is inert;
    // the full UTF-8 proof is still required before rewriting.
    let text = String::from_utf8_lossy(prefix_bytes);
    let lowered = text
        .trim_start_matches('\u{feff}')
        .trim_start()
        .to_ascii_lowercase();
    let mut prefix = lowered.as_str();
    while let Some(comment) = prefix.strip_prefix("<!--") {
        let (_, tail) = comment.split_once("-->")?;
        prefix = tail.trim_start();
    }
    if prefix.starts_with('<') {
        Some(RewriteKind::Html)
    } else {
        None
    }
}

impl<T> ConcurrentWebviewGateway<T> {
    /// Build a concurrent webview gateway around `transport`.
    pub fn new(prefix: GatewayPrefix, transport: T) -> Self {
        Self {
            policy: GatewayResponsePolicy::new(prefix),
            transport,
            cookies: Mutex::new(CookieJar::new()),
        }
    }

    /// Attach a runtime bootstrap script that is injected into rendered HTML.
    pub fn with_bootstrap_script(mut self, script: impl Into<String>) -> Self {
        self.policy.bootstrap_script = Some(BootstrapScript::Static(script.into()));
        self
    }

    /// Attach a runtime bootstrap factory evaluated against each rendered request.
    pub fn with_request_bootstrap(mut self, script: fn(&GatewayRequest) -> String) -> Self {
        self.policy.bootstrap_script = Some(BootstrapScript::PerRequest(script));
        self
    }
}

impl<T> ConcurrentWebviewGateway<T>
where T: GatewayTransport
{
    /// Send one request without holding the virtual-cookie lock during upstream I/O.
    pub async fn send(&self, request: GatewayRequest) -> Result<GatewayResponse> {
        // `GatewayRequest` remains a wire-friendly type with a public `Url` field. Enforce the
        // same admission boundary here so direct Rust callers cannot bypass `TargetUrl` and hand
        // a loopback or private destination to an otherwise naive transport implementation.
        TargetUrl::from_url(request.target.clone())?;
        let mut session = {
            let prepare_now = current_time_millis()?;
            let cookies = self.lock_cookies()?;
            GatewayRequestSession::prepare(&self.policy, &cookies, request, prepare_now)?
        };
        let response = session.send(&self.policy, &self.transport).await?;
        {
            let commit_now = current_time_millis()?;
            let mut cookies = self.lock_cookies()?;
            session.commit_response_cookies(&self.policy, &mut cookies, &response, commit_now)?;
        }
        session.finish(&self.policy, response)
    }

    /// Lock the shared virtual cookie jar, mapping lock poisoning to a typed error.
    fn lock_cookies(&self) -> Result<std::sync::MutexGuard<'_, CookieJar>> {
        self.cookies
            .lock()
            .map_err(|_| crate::error::CookieFailure::JarLockPoisoned.into())
    }
}

/// Read the current wall-clock time as Unix-epoch milliseconds for cookie expiration checks.
fn current_time_millis() -> Result<i64> {
    #[cfg(all(target_family = "wasm", feature = "browser"))]
    {
        Ok(js_sys::Date::now() as i64)
    }

    #[cfg(all(target_family = "wasm", not(feature = "browser")))]
    {
        Err(crate::error::TransportFailure::ClockUnavailable.into())
    }

    #[cfg(not(target_family = "wasm"))]
    {
        Ok(system_time_millis(SystemTime::now()))
    }
}

/// Convert a native `SystemTime` into saturating Unix-epoch milliseconds.
#[cfg(not(target_family = "wasm"))]
fn system_time_millis(time: SystemTime) -> i64 {
    let Ok(duration) = time.duration_since(UNIX_EPOCH) else {
        return 0;
    };
    let millis = duration.as_millis();
    if millis > i64::MAX as u128 {
        i64::MAX
    } else {
        millis as i64
    }
}

#[cfg(test)]
mod tests;
