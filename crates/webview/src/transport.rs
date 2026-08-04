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

/// Pluggable transport for normalized webview gateway requests.
#[async_trait(?Send)]
pub trait GatewayTransport {
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

/// Policy wrapper that applies cookies, header policy, and body rewriting around a transport.
pub struct WebviewGateway<T> {
    policy: GatewayResponsePolicy,
    transport: T,
    cookies: CookieJar,
}

/// A gateway that permits concurrent transport requests while sharing one virtual cookie jar.
///
/// The cookie jar is locked only while a request is prepared and while upstream `Set-Cookie`
/// headers are committed. Network I/O and response rewriting run outside that lock, so one slow
/// upstream request cannot block unrelated page resources.
pub struct ConcurrentWebviewGateway<T> {
    policy: GatewayResponsePolicy,
    transport: T,
    cookies: Mutex<CookieJar>,
}

struct GatewayResponsePolicy {
    prefix: GatewayPrefix,
    header_policy: HeaderPolicy,
    bootstrap_script: Option<BootstrapScript>,
}

/// One request after its cookie-dependent preparation and before its cookie-dependent commit.
///
/// The session keeps the two synchronization points explicit: a caller prepares it while it can
/// read the virtual jar, then commits response cookies after transport I/O. Everything between
/// those points is independent of jar ownership and is therefore shared by both gateway
/// adapters.
struct GatewayRequestSession {
    cors_request: GatewayRequest,
    request: Option<GatewayRequest>,
    preflight: Option<GatewayRequest>,
    target: url::Url,
    stores_cookies: bool,
}

impl GatewayRequestSession {
    fn prepare(
        policy: &GatewayResponsePolicy,
        cookies: &CookieJar,
        request: GatewayRequest,
        now: i64,
    ) -> Result<Self> {
        validate_request_body_size(&request)?;
        let cors_request = request.clone();
        let preflight = cors::preflight_request(&request)?;
        let request = policy.prepare_request(cookies, request, now)?;
        let target = request.target.clone();
        let stores_cookies = request.allows_target_cookies();

        Ok(Self {
            cors_request,
            request: Some(request),
            preflight,
            target,
            stores_cookies,
        })
    }

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

    fn commit_response_cookies(
        &self,
        policy: &GatewayResponsePolicy,
        cookies: &mut CookieJar,
        response: &GatewayResponse,
        now: i64,
    ) -> Result<()> {
        if self.stores_cookies {
            policy.store_response_cookies(cookies, &self.target, response, now)?;
        }
        Ok(())
    }

    fn finish(
        &self,
        policy: &GatewayResponsePolicy,
        response: GatewayResponse,
    ) -> Result<GatewayResponse> {
        let response = policy.finish_response(&self.cors_request, response)?;
        validate_response_body_size(&response, GatewayResponseBodyLimit::DEFAULT)?;
        Ok(response)
    }
}

enum BootstrapScript {
    Static(String),
    PerTarget(fn(&url::Url) -> String),
    PerRequest(fn(&GatewayRequest) -> String),
}

impl GatewayResponsePolicy {
    fn new(prefix: GatewayPrefix) -> Self {
        Self {
            header_policy: HeaderPolicy::new(prefix.clone()),
            prefix,
            bootstrap_script: None,
        }
    }

    fn prepare_request(
        &self,
        cookies: &CookieJar,
        request: GatewayRequest,
        now: i64,
    ) -> Result<GatewayRequest> {
        let request = request.normalize_source_origin();
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

    fn store_response_cookies(
        &self,
        cookies: &mut CookieJar,
        target: &url::Url,
        response: &GatewayResponse,
        now: i64,
    ) -> Result<()> {
        for header in response
            .headers
            .iter()
            .filter(|header| header.name_eq("set-cookie"))
        {
            cookies.store_set_cookie_at(target, header.value.as_str(), now)?;
        }
        Ok(())
    }

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
        let rewrite_kind = content_type
            .as_deref()
            .and_then(rewrite_kind_for_media_type)
            .or_else(|| sniff_active_document(response.body.as_slice()));
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
            ctx = ctx.with_bootstrap_script(script);
        }
        let rewritten = match rewrite_kind {
            RewriteKind::Html => ctx.rewrite_html_with_limit(text, MAX_GATEWAY_BODY_BYTES)?,
            RewriteKind::Css => ctx.rewrite_css_with_limit(text, MAX_GATEWAY_BODY_BYTES)?,
        };
        Ok(rewritten.into_bytes())
    }

    fn bootstrap_for(&self, request: &GatewayRequest) -> Option<String> {
        match &self.bootstrap_script {
            Some(BootstrapScript::Static(script)) => Some(script.clone()),
            Some(BootstrapScript::PerTarget(build)) => Some(build(&request.target)),
            Some(BootstrapScript::PerRequest(build)) => Some(build(request)),
            None => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RewriteKind {
    Html,
    Css,
}

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

fn normalize_media_type(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn rewrite_kind_for_media_type(media_type: &str) -> Option<RewriteKind> {
    match media_type {
        "text/html" | "application/xhtml+xml" | "image/svg+xml" => Some(RewriteKind::Html),
        "text/css" => Some(RewriteKind::Css),
        _ => None,
    }
}

fn is_inert_navigation_media_type(media_type: &str) -> bool {
    matches!(
        media_type,
        "text/plain"
            | "application/json"
            | "application/pdf"
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

// These two adapters differ only in cookie ownership and the resulting `send` receiver. Keep the
// policy-facing API generated from one definition so adding a bootstrap or path rule cannot drift
// between the serialized and concurrent shells.
macro_rules! impl_gateway_policy_api {
    ($gateway:ident) => {
        impl<T> $gateway<T> {
            /// Return the controlled-origin gateway prefix.
            pub fn prefix(&self) -> &GatewayPrefix {
                &self.policy.prefix
            }

            /// Attach a runtime bootstrap script that is injected into rendered HTML.
            pub fn with_bootstrap_script(mut self, script: impl Into<String>) -> Self {
                self.policy.bootstrap_script = Some(BootstrapScript::Static(script.into()));
                self
            }

            /// Attach a runtime bootstrap factory evaluated for each rendered target page.
            pub fn with_target_bootstrap(mut self, script: fn(&url::Url) -> String) -> Self {
                self.policy.bootstrap_script = Some(BootstrapScript::PerTarget(script));
                self
            }

            /// Attach a runtime bootstrap factory evaluated against each rendered request.
            pub fn with_request_bootstrap(mut self, script: fn(&GatewayRequest) -> String) -> Self {
                self.policy.bootstrap_script = Some(BootstrapScript::PerRequest(script));
                self
            }

            /// Build a typed request from a controlled-origin gateway path.
            pub fn request_from_gateway_path(
                &self,
                path: &str,
                kind: GatewayRequestKind,
            ) -> Result<GatewayRequest> {
                let target = self.policy.prefix.decode_path(path)?.into_url();
                Ok(match kind {
                    GatewayRequestKind::Navigation => GatewayRequest::navigation(target),
                    GatewayRequestKind::Subresource => GatewayRequest::subresource(target),
                    GatewayRequestKind::Fetch | GatewayRequestKind::Xhr => {
                        return Err(WebviewError::MissingRuntimeSourceOrigin);
                    }
                })
            }
        }
    };
}

impl_gateway_policy_api!(WebviewGateway);
impl_gateway_policy_api!(ConcurrentWebviewGateway);

impl<T> WebviewGateway<T>
where T: GatewayTransport
{
    /// Build a webview gateway around `transport`.
    pub fn new(prefix: GatewayPrefix, transport: T) -> Self {
        Self {
            policy: GatewayResponsePolicy::new(prefix),
            transport,
            cookies: CookieJar::new(),
        }
    }

    /// Access the virtual cookie jar.
    pub fn cookies(&self) -> &CookieJar {
        &self.cookies
    }

    /// Send one request through the gateway policy stack.
    pub async fn send(&mut self, request: GatewayRequest) -> Result<GatewayResponse> {
        let prepare_now = current_time_millis()?;
        let mut session =
            GatewayRequestSession::prepare(&self.policy, &self.cookies, request, prepare_now)?;
        let response = session.send(&self.policy, &self.transport).await?;
        let commit_now = current_time_millis()?;
        session.commit_response_cookies(&self.policy, &mut self.cookies, &response, commit_now)?;
        session.finish(&self.policy, response)
    }

    /// Send one request addressed by a controlled-origin gateway path.
    pub async fn send_gateway_path(
        &mut self,
        path: &str,
        kind: GatewayRequestKind,
    ) -> Result<GatewayResponse> {
        let request = self.request_from_gateway_path(path, kind)?;
        self.send(request).await
    }
}

impl<T> ConcurrentWebviewGateway<T>
where T: GatewayTransport
{
    /// Build a concurrent webview gateway around `transport`.
    pub fn new(prefix: GatewayPrefix, transport: T) -> Self {
        Self {
            policy: GatewayResponsePolicy::new(prefix),
            transport,
            cookies: Mutex::new(CookieJar::new()),
        }
    }

    /// Send one request without holding the virtual-cookie lock during upstream I/O.
    pub async fn send(&self, request: GatewayRequest) -> Result<GatewayResponse> {
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

    /// Send one request addressed by a controlled-origin gateway path.
    pub async fn send_gateway_path(
        &self,
        path: &str,
        kind: GatewayRequestKind,
    ) -> Result<GatewayResponse> {
        let request = self.request_from_gateway_path(path, kind)?;
        self.send(request).await
    }

    fn lock_cookies(&self) -> Result<std::sync::MutexGuard<'_, CookieJar>> {
        self.cookies
            .lock()
            .map_err(|_| crate::error::CookieFailure::JarLockPoisoned.into())
    }
}

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
