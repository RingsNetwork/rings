use std::sync::Mutex;
#[cfg(not(all(target_family = "wasm", feature = "browser")))]
use std::time::SystemTime;
#[cfg(not(all(target_family = "wasm", feature = "browser")))]
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

/// Pluggable transport for normalized webview gateway requests.
#[async_trait(?Send)]
pub trait GatewayTransport {
    /// Send `request` through the concrete transport and return a raw response.
    async fn send(&self, request: GatewayRequest) -> Result<GatewayResponse>;
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
            let response = transport.send(preflight).await?;
            cors::validate_preflight_response(&self.cors_request, &response)?;
        }
        let request = self
            .request
            .take()
            .ok_or(crate::error::TransportFailure::RequestSessionSentTwice)?;
        let response = transport.send(request).await?;
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
        policy.finish_response(&self.cors_request, response)
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
        let Some(content_type) = response
            .headers
            .iter()
            .find(|header| header.name_eq("content-type"))
            .map(|header| header.value.to_ascii_lowercase())
        else {
            return Ok(response.body.clone());
        };
        if !(content_type.contains("text/html") || content_type.contains("text/css")) {
            return Ok(response.body.clone());
        }
        let is_html = content_type.contains("text/html");
        let text = std::str::from_utf8(response.body.as_slice()).map_err(|_| {
            // Rewriting is the boundary that keeps navigable text on the controlled origin.
            // Passing an undecodable document through would bypass that boundary.
            WebviewError::UnrewritableTextEncoding { content_type }
        })?;
        let mut ctx = RewriteContext::new(self.prefix.clone(), request.target.clone());
        if let Some(script) = self.bootstrap_for(request) {
            ctx = ctx.with_bootstrap_script(script);
        }
        let rewritten = if is_html {
            ctx.rewrite_html(text)?
        } else {
            ctx.rewrite_css(text)?
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
    ///
    /// Use this when the bootstrap resolves relative runtime URLs against the target document.
    /// The factory is evaluated after the upstream response is received and before its HTML is
    /// rewritten, so navigating between targets cannot retain an earlier page's base URL.
    pub fn with_target_bootstrap(mut self, script: fn(&url::Url) -> String) -> Self {
        self.policy.bootstrap_script = Some(BootstrapScript::PerTarget(script));
        self
    }

    /// Attach a runtime bootstrap factory evaluated against each rendered gateway request.
    pub fn with_request_bootstrap(mut self, script: fn(&GatewayRequest) -> String) -> Self {
        self.policy.bootstrap_script = Some(BootstrapScript::PerRequest(script));
        self
    }

    /// Access the virtual cookie jar.
    pub fn cookies(&self) -> &CookieJar {
        &self.cookies
    }

    /// Send one request through the gateway policy stack.
    pub async fn send(&mut self, request: GatewayRequest) -> Result<GatewayResponse> {
        let prepare_now = current_time_millis();
        let mut session =
            GatewayRequestSession::prepare(&self.policy, &self.cookies, request, prepare_now)?;
        let response = session.send(&self.policy, &self.transport).await?;
        let commit_now = current_time_millis();
        session.commit_response_cookies(&self.policy, &mut self.cookies, &response, commit_now)?;
        session.finish(&self.policy, response)
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
                return Err(WebviewError::MissingRuntimeSourceOrigin)
            }
        })
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

    /// Attach a runtime bootstrap factory evaluated against each rendered gateway request.
    pub fn with_request_bootstrap(mut self, script: fn(&GatewayRequest) -> String) -> Self {
        self.policy.bootstrap_script = Some(BootstrapScript::PerRequest(script));
        self
    }

    /// Send one request without holding the virtual-cookie lock during upstream I/O.
    pub async fn send(&self, request: GatewayRequest) -> Result<GatewayResponse> {
        let mut session = {
            let prepare_now = current_time_millis();
            let cookies = self.lock_cookies()?;
            GatewayRequestSession::prepare(&self.policy, &cookies, request, prepare_now)?
        };
        let response = session.send(&self.policy, &self.transport).await?;
        {
            let commit_now = current_time_millis();
            let mut cookies = self.lock_cookies()?;
            session.commit_response_cookies(&self.policy, &mut cookies, &response, commit_now)?;
        }
        session.finish(&self.policy, response)
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
                return Err(WebviewError::MissingRuntimeSourceOrigin)
            }
        })
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

fn current_time_millis() -> i64 {
    #[cfg(all(target_family = "wasm", feature = "browser"))]
    {
        js_sys::Date::now() as i64
    }

    #[cfg(not(all(target_family = "wasm", feature = "browser")))]
    {
        system_time_millis(SystemTime::now())
    }
}

#[cfg(not(all(target_family = "wasm", feature = "browser")))]
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
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use futures::channel::mpsc;
    use futures::channel::oneshot;
    use futures::executor::LocalPool;
    use futures::stream::StreamExt;
    use futures::task::LocalSpawnExt;
    use url::Url;

    use super::*;
    use crate::types::GatewayCredentials;
    use crate::types::GatewayRequest;
    use crate::types::GatewayRequestKind;
    use crate::url::TargetUrl;
    use crate::WebviewError;

    struct StaticTransport;

    #[async_trait(?Send)]
    impl GatewayTransport for StaticTransport {
        async fn send(&self, request: GatewayRequest) -> Result<GatewayResponse> {
            assert!(request
                .headers
                .iter()
                .all(|header| !header.name_eq("cookie")));
            GatewayResponse::new(
                200,
                vec![
                    GatewayHeader::new("Content-Type", "text/html")?,
                    GatewayHeader::new("Set-Cookie", "sid=one; Path=/")?,
                ],
                br#"<img src="/asset.png">"#.to_vec(),
            )
        }
    }

    #[test]
    fn gateway_rewrites_html_and_stores_cookies() -> Result<()> {
        let target = TargetUrl::parse("https://example.com/index.html")?.into_url();
        let mut gateway = WebviewGateway::new(GatewayPrefix::new("/webview/")?, StaticTransport)
            .with_bootstrap_script("globalThis.__rings = true;");
        let request = GatewayRequest {
            target,
            method: "GET".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            kind: GatewayRequestKind::Navigation,
            source_origin: None,
            source_target: None,
            credentials: GatewayCredentials::SameOrigin,
            top_level_navigation: true,
        };

        let response = futures::executor::block_on(gateway.send(request))?;
        let body = String::from_utf8(response.body)
            .map_err(|error| WebviewError::transport(error.to_string()))?;

        assert!(body.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fasset%2Epng"));
        assert!(body.contains("data-rings-webview-bootstrap"));
        assert_eq!(gateway.cookies().len(), 1);
        assert!(!response
            .headers
            .iter()
            .any(|header| header.name_eq("set-cookie")));
        Ok(())
    }

    struct InvalidUtf8TextTransport(&'static str);

    #[async_trait(?Send)]
    impl GatewayTransport for InvalidUtf8TextTransport {
        async fn send(&self, _request: GatewayRequest) -> Result<GatewayResponse> {
            GatewayResponse::new(
                200,
                vec![GatewayHeader::new("Content-Type", self.0)?],
                vec![b'<', 0xff, b'>'],
            )
        }
    }

    #[test]
    fn gateway_rejects_non_utf8_rewritable_documents() -> Result<()> {
        let target = TargetUrl::parse("https://example.com/index.html")?.into_url();
        for content_type in [
            "text/html; charset=iso-8859-1",
            "text/css; charset=iso-8859-1",
        ] {
            let mut gateway = WebviewGateway::new(
                GatewayPrefix::new("/webview/")?,
                InvalidUtf8TextTransport(content_type),
            );
            let result = futures::executor::block_on(
                gateway.send(GatewayRequest::navigation(target.clone())),
            );

            assert!(matches!(
                result,
                Err(WebviewError::UnrewritableTextEncoding { content_type: actual })
                    if actual == content_type
            ));
        }
        Ok(())
    }

    fn target_bootstrap(target: &url::Url) -> String {
        format!("globalThis.__ringsTarget = {:?};", target.as_str())
    }

    fn request_bootstrap(request: &GatewayRequest) -> String {
        format!(
            "globalThis.__ringsTopLevelNavigation = {};",
            request.top_level_navigation
        )
    }

    #[test]
    fn per_target_bootstrap_tracks_each_navigated_document() -> Result<()> {
        let mut gateway = WebviewGateway::new(GatewayPrefix::new("/webview/")?, StaticTransport)
            .with_target_bootstrap(target_bootstrap);
        let first = TargetUrl::parse("https://one.example.test/first")?.into_url();
        let second = TargetUrl::parse("https://two.example.test/second")?.into_url();

        let first = futures::executor::block_on(gateway.send(GatewayRequest::navigation(first)))?;
        let second = futures::executor::block_on(gateway.send(GatewayRequest::navigation(second)))?;
        let first_body = String::from_utf8(first.body)
            .map_err(|error| WebviewError::transport(error.to_string()))?;
        let second_body = String::from_utf8(second.body)
            .map_err(|error| WebviewError::transport(error.to_string()))?;

        assert!(first_body.contains("https://one.example.test/first"));
        assert!(!first_body.contains("https://two.example.test/second"));
        assert!(second_body.contains("https://two.example.test/second"));
        assert!(!second_body.contains("https://one.example.test/first"));
        Ok(())
    }

    #[test]
    fn per_request_bootstrap_tracks_navigation_context() -> Result<()> {
        let mut gateway = WebviewGateway::new(GatewayPrefix::new("/webview/")?, StaticTransport)
            .with_request_bootstrap(request_bootstrap);
        let target = TargetUrl::parse("https://frame.example.test/nested")?.into_url();
        let request = GatewayRequest::navigation(target).with_top_level_navigation(false);

        let response = futures::executor::block_on(gateway.send(request))?;
        let body = String::from_utf8(response.body)
            .map_err(|error| WebviewError::transport(error.to_string()))?;

        assert!(body.contains("globalThis.__ringsTopLevelNavigation = false;"));
        Ok(())
    }

    #[test]
    fn source_free_runtime_gateway_requests_are_rejected() -> Result<()> {
        let target = TargetUrl::parse("https://api.example.test/data")?.into_url();
        let mut gateway = WebviewGateway::new(GatewayPrefix::new("/webview/")?, StaticTransport);

        let send = futures::executor::block_on(gateway.send(GatewayRequest::fetch(target, "GET")));
        assert!(matches!(
            send,
            Err(WebviewError::MissingRuntimeSourceOrigin)
        ));
        assert!(matches!(
            gateway.request_from_gateway_path(
                "/webview/https%3A%2F%2Fapi%2Eexample%2Etest%2Fdata",
                GatewayRequestKind::Fetch,
            ),
            Err(WebviewError::MissingRuntimeSourceOrigin)
        ));
        Ok(())
    }

    struct DomainCookieTransport;

    #[async_trait(?Send)]
    impl GatewayTransport for DomainCookieTransport {
        async fn send(&self, _request: GatewayRequest) -> Result<GatewayResponse> {
            GatewayResponse::new(
                200,
                vec![
                    GatewayHeader::new("Content-Type", "text/html")?,
                    GatewayHeader::new("Set-Cookie", "sid=domain; Domain=example.com; Path=/")?,
                ],
                br#"<p>ok</p>"#.to_vec(),
            )
        }
    }

    #[test]
    fn gateway_ignores_domain_cookie_without_failing_response() -> Result<()> {
        let target = TargetUrl::parse("https://example.com/index.html")?.into_url();
        let mut gateway =
            WebviewGateway::new(GatewayPrefix::new("/webview/")?, DomainCookieTransport);

        let response =
            futures::executor::block_on(gateway.send(GatewayRequest::navigation(target)))?;
        let body = String::from_utf8(response.body)
            .map_err(|error| WebviewError::transport(error.to_string()))?;

        assert!(body.contains("<p>ok</p>"));
        assert!(gateway.cookies().is_empty());
        assert!(!response
            .headers
            .iter()
            .any(|header| header.name_eq("set-cookie")));
        Ok(())
    }

    struct RecordingTransport {
        requests: std::cell::RefCell<Vec<GatewayRequest>>,
    }

    impl RecordingTransport {
        fn new() -> Self {
            Self {
                requests: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    #[async_trait(?Send)]
    impl GatewayTransport for RecordingTransport {
        async fn send(&self, request: GatewayRequest) -> Result<GatewayResponse> {
            self.requests.borrow_mut().push(request);
            GatewayResponse::new(
                200,
                vec![
                    GatewayHeader::new("Content-Type", "application/json")?,
                    GatewayHeader::new("Set-Cookie", "sid=one; Path=/")?,
                ],
                br#"{}"#.to_vec(),
            )
        }
    }

    #[test]
    fn gateway_replaces_caller_cookie_header_with_virtual_target_cookie() -> Result<()> {
        let transport = RecordingTransport::new();
        let target = TargetUrl::parse("https://example.com/index.html")?.into_url();
        let fetch_target = TargetUrl::parse("https://example.com/api")?.into_url();
        let mut gateway = WebviewGateway::new(GatewayPrefix::new("/webview/")?, transport);

        futures::executor::block_on(gateway.send(GatewayRequest::navigation(target)))?;
        futures::executor::block_on(
            gateway.send(
                GatewayRequest::fetch(fetch_target, "GET")
                    .with_source_origin(
                        TargetUrl::parse("https://example.com/index.html")?.into_url(),
                    )
                    .with_header(GatewayHeader::new("Cookie", "caller=leak")?),
            ),
        )?;

        let requests = gateway.transport.requests.borrow();
        let second = requests
            .get(1)
            .ok_or_else(|| WebviewError::transport("missing second request".to_string()))?;
        let cookies: Vec<&str> = second
            .headers
            .iter()
            .filter(|header| header.name_eq("cookie"))
            .map(|header| header.value.as_str())
            .collect();

        assert_eq!(cookies, vec!["sid=one"]);
        Ok(())
    }

    #[test]
    fn gateway_normalizes_direct_struct_source_origin_before_transport() -> Result<()> {
        let transport = RecordingTransport::new();
        let request = GatewayRequest {
            target: TargetUrl::parse("https://app.example.test:8443/data")?.into_url(),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            kind: GatewayRequestKind::Fetch,
            source_origin: Some(Url::parse(
                "https://user:pass@app.example.test:8443/page?q=1#section",
            )?),
            source_target: None,
            credentials: GatewayCredentials::SameOrigin,
            top_level_navigation: false,
        };
        let mut gateway = WebviewGateway::new(GatewayPrefix::new("/webview/")?, transport);

        futures::executor::block_on(gateway.send(request))?;

        let requests = gateway.transport.requests.borrow();
        let first = requests
            .first()
            .ok_or_else(|| WebviewError::transport("missing request".to_string()))?;
        assert_eq!(
            first.source_origin.as_ref().map(Url::as_str),
            Some("https://app.example.test:8443/")
        );
        Ok(())
    }

    #[test]
    fn gateway_strips_controlled_origin_headers_before_transport() -> Result<()> {
        let transport = RecordingTransport::new();
        let target = TargetUrl::parse("https://example.com/index.html")?.into_url();
        let mut gateway = WebviewGateway::new(GatewayPrefix::new("/webview/")?, transport);

        futures::executor::block_on(
            gateway.send(
                GatewayRequest::navigation(target)
                    .with_header(GatewayHeader::new("Host", "127.0.0.1:3000")?)
                    .with_header(GatewayHeader::new("Origin", "http://127.0.0.1:3000")?)
                    .with_header(GatewayHeader::new(
                        "Referer",
                        "http://127.0.0.1:3000/webview/target",
                    )?)
                    .with_header(GatewayHeader::new("Sec-Fetch-Dest", "document")?)
                    .with_header(GatewayHeader::new("Accept", "text/html")?),
            ),
        )?;

        let requests = gateway.transport.requests.borrow();
        let first = requests
            .first()
            .ok_or_else(|| WebviewError::transport("missing first request".to_string()))?;
        assert!(first.headers.iter().all(|header| {
            !header.name_eq("host")
                && !header.name_eq("origin")
                && !header.name_eq("referer")
                && !header.name_eq("sec-fetch-dest")
        }));
        assert!(first
            .headers
            .iter()
            .any(|header| header.name_eq("accept") && header.value == "text/html"));
        Ok(())
    }

    struct CorsRecordingTransport {
        requests: std::cell::RefCell<Vec<GatewayRequest>>,
    }

    #[async_trait(?Send)]
    impl GatewayTransport for CorsRecordingTransport {
        async fn send(&self, request: GatewayRequest) -> Result<GatewayResponse> {
            let is_preflight = request.method == "OPTIONS";
            self.requests.borrow_mut().push(request);
            let mut headers = vec![GatewayHeader::new(
                "Access-Control-Allow-Origin",
                "https://app.example.test",
            )?];
            if is_preflight {
                headers.push(GatewayHeader::new("Access-Control-Allow-Methods", "PATCH")?);
                headers.push(GatewayHeader::new(
                    "Access-Control-Allow-Headers",
                    "x-requested-with",
                )?);
            }
            GatewayResponse::new(200, headers, b"cors response".to_vec())
        }
    }

    #[test]
    fn gateway_forwards_cross_origin_runtime_requests_after_virtual_cors_preflight() -> Result<()> {
        let target = TargetUrl::parse("https://api.example.test/data")?.into_url();
        let source = TargetUrl::parse("https://app.example.test/page")?.into_url();
        let transport = CorsRecordingTransport {
            requests: std::cell::RefCell::new(Vec::new()),
        };
        let mut gateway = WebviewGateway::new(GatewayPrefix::new("/webview/")?, transport);
        let response = futures::executor::block_on(
            gateway.send(
                GatewayRequest::fetch(target, "PATCH")
                    .with_source_origin(source)
                    .with_header(GatewayHeader::new("X-Requested-With", "Rings")?),
            ),
        )?;

        assert_eq!(response.body, b"cors response");
        let requests = gateway.transport.requests.borrow();
        assert_eq!(requests.len(), 2);
        let preflight = requests
            .first()
            .ok_or_else(|| WebviewError::transport("missing CORS preflight".to_string()))?;
        assert_eq!(preflight.method, "OPTIONS");
        assert!(preflight.headers.iter().any(|header| {
            header.name_eq("origin") && header.value == "https://app.example.test"
        }));
        assert!(preflight.headers.iter().any(|header| {
            header.name_eq("access-control-request-method") && header.value == "PATCH"
        }));
        let actual = requests
            .get(1)
            .ok_or_else(|| WebviewError::transport("missing CORS runtime request".to_string()))?;
        assert_eq!(actual.method, "PATCH");
        assert!(actual.headers.iter().any(|header| {
            header.name_eq("origin") && header.value == "https://app.example.test"
        }));
        Ok(())
    }

    struct ParityTransport {
        requests: RefCell<Vec<GatewayRequest>>,
    }

    impl ParityTransport {
        fn new() -> Self {
            Self {
                requests: RefCell::new(Vec::new()),
            }
        }
    }

    #[async_trait(?Send)]
    impl GatewayTransport for ParityTransport {
        async fn send(&self, request: GatewayRequest) -> Result<GatewayResponse> {
            let is_preflight = request.method == "OPTIONS";
            self.requests.borrow_mut().push(request);
            let mut headers = vec![GatewayHeader::new(
                "Access-Control-Allow-Origin",
                "https://app.example.test",
            )?];
            if is_preflight {
                headers.push(GatewayHeader::new("Access-Control-Allow-Methods", "PATCH")?);
                headers.push(GatewayHeader::new(
                    "Access-Control-Allow-Headers",
                    "x-requested-with",
                )?);
            } else {
                headers.push(GatewayHeader::new("Set-Cookie", "sid=one; Path=/")?);
            }
            GatewayResponse::new(200, headers, b"parity response".to_vec())
        }
    }

    fn parity_request_sequence() -> Result<Vec<GatewayRequest>> {
        let app = TargetUrl::parse("https://app.example.test/page")?.into_url();
        let api = TargetUrl::parse("https://api.example.test/data")?.into_url();
        Ok(vec![
            GatewayRequest::navigation(app.clone()),
            GatewayRequest::fetch(api, "PATCH")
                .with_source_origin(app.clone())
                .with_header(GatewayHeader::new("X-Requested-With", "Rings")?),
            GatewayRequest::fetch(app, "GET").with_source_origin(
                TargetUrl::parse("https://app.example.test/other-page")?.into_url(),
            ),
        ])
    }

    #[test]
    fn gateway_adapters_share_cookie_cors_and_response_policy() -> Result<()> {
        let prefix = GatewayPrefix::new("/webview/")?;
        let mut single = WebviewGateway::new(prefix.clone(), ParityTransport::new());
        let concurrent = ConcurrentWebviewGateway::new(prefix, ParityTransport::new());
        let requests = parity_request_sequence()?;

        let mut single_responses = Vec::new();
        for request in requests.clone() {
            single_responses.push(futures::executor::block_on(single.send(request))?);
        }
        let single_requests = single.transport.requests.borrow().clone();
        let single_cookie_count = single.cookies().len();

        let mut concurrent_responses = Vec::new();
        for request in requests {
            concurrent_responses.push(futures::executor::block_on(concurrent.send(request))?);
        }
        let concurrent_requests = concurrent.transport.requests.borrow().clone();
        let concurrent_cookie_count = concurrent.lock_cookies()?.len();

        assert_eq!(single_requests, concurrent_requests);
        assert_eq!(single_responses, concurrent_responses);
        assert_eq!(single_cookie_count, concurrent_cookie_count);
        assert_eq!(single_cookie_count, 1);
        let final_request = single_requests.last().ok_or_else(|| {
            WebviewError::transport("parity transport did not receive final request".to_string())
        })?;
        assert!(final_request
            .headers
            .iter()
            .any(|header| header.name_eq("cookie") && header.value == "sid=one"));
        Ok(())
    }

    struct SlowFirstTransport {
        started: mpsc::UnboundedSender<String>,
        release_slow_request: RefCell<Option<oneshot::Receiver<()>>>,
    }

    #[async_trait(?Send)]
    impl GatewayTransport for SlowFirstTransport {
        async fn send(&self, request: GatewayRequest) -> Result<GatewayResponse> {
            let path = request.target.path().to_string();
            let _ = self.started.unbounded_send(path.clone());
            if path == "/slow" {
                let receiver = self
                    .release_slow_request
                    .borrow_mut()
                    .take()
                    .ok_or_else(|| {
                        WebviewError::transport("slow request was released twice".to_string())
                    })?;
                receiver.await.map_err(|_| {
                    WebviewError::transport("slow request release channel was dropped".to_string())
                })?;
            }
            GatewayResponse::new(
                200,
                vec![GatewayHeader::new("content-type", "text/plain")?],
                path.into_bytes(),
            )
        }
    }

    #[test]
    fn concurrent_gateway_allows_fast_resource_while_slow_resource_waits() -> Result<()> {
        let (started_sender, mut started_receiver) = mpsc::unbounded();
        let (release_slow_sender, release_slow_receiver) = oneshot::channel();
        let gateway = Rc::new(ConcurrentWebviewGateway::new(
            GatewayPrefix::new("/webview/")?,
            SlowFirstTransport {
                started: started_sender,
                release_slow_request: RefCell::new(Some(release_slow_receiver)),
            },
        ));
        let slow = TargetUrl::parse("https://example.test/slow")?.into_url();
        let fast = TargetUrl::parse("https://example.test/fast")?.into_url();
        let (slow_result_sender, slow_result_receiver) = oneshot::channel();
        let (fast_result_sender, fast_result_receiver) = oneshot::channel();
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();

        let slow_gateway = Rc::clone(&gateway);
        spawner
            .spawn_local(async move {
                let _ = slow_result_sender
                    .send(slow_gateway.send(GatewayRequest::navigation(slow)).await);
            })
            .map_err(|error| WebviewError::transport(error.to_string()))?;
        assert_eq!(
            pool.run_until(started_receiver.next()),
            Some("/slow".to_string())
        );

        let fast_gateway = Rc::clone(&gateway);
        spawner
            .spawn_local(async move {
                let _ = fast_result_sender
                    .send(fast_gateway.send(GatewayRequest::subresource(fast)).await);
            })
            .map_err(|error| WebviewError::transport(error.to_string()))?;
        let fast_response = pool
            .run_until(fast_result_receiver)
            .map_err(|_| WebviewError::transport("fast resource task was dropped".to_string()))??;
        assert_eq!(fast_response.body, b"/fast");

        release_slow_sender.send(()).map_err(|_| {
            WebviewError::transport("slow resource task stopped waiting unexpectedly".to_string())
        })?;
        let slow_response = pool
            .run_until(slow_result_receiver)
            .map_err(|_| WebviewError::transport("slow resource task was dropped".to_string()))??;
        assert_eq!(slow_response.body, b"/slow");
        Ok(())
    }
}
