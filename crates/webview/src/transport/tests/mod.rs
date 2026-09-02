use std::rc::Rc;
use std::sync::Mutex;

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

mod test_xhtml;

trait TestMutex<T> {
    fn test_lock(&self) -> Result<std::sync::MutexGuard<'_, T>>;
}

impl<T> TestMutex<T> for Mutex<T> {
    fn test_lock(&self) -> Result<std::sync::MutexGuard<'_, T>> {
        self.lock()
            .map_err(|_| WebviewError::transport("test transport lock poisoned".to_string()))
    }
}

struct StaticTransport;

#[cfg(not(target_family = "wasm"))]
#[test]
fn test_native_gateway_transport_preserves_send_and_sync_bounds() {
    fn assert_send_sync<T: GatewayTransport + Send + Sync>() {}
    assert_send_sync::<StaticTransport>();
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl GatewayTransport for StaticTransport {
    async fn send(
        &self,
        request: GatewayRequest,
        _body_limit: GatewayResponseBodyLimit,
    ) -> Result<GatewayResponse> {
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

#[cfg(all(target_family = "wasm", not(feature = "browser")))]
#[test]
fn test_wasm_without_browser_reports_clock_unavailable_before_transport_io() -> Result<()> {
    let gateway = ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, StaticTransport);
    let request = GatewayRequest::navigation(Url::parse("https://example.test/")?);

    assert!(matches!(
        futures::executor::block_on(gateway.send(request)),
        Err(WebviewError::Transport(
            crate::error::TransportFailure::ClockUnavailable
        ))
    ));
    Ok(())
}

#[test]
fn test_gateway_rewrites_html_and_stores_cookies() -> Result<()> {
    let target = TargetUrl::parse("https://example.com/index.html")?.into_url();
    let gateway = ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, StaticTransport)
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
    assert_eq!(gateway.lock_cookies()?.len(), 1);
    assert!(!response
        .headers
        .iter()
        .any(|header| header.name_eq("set-cookie")));
    Ok(())
}

struct InvalidUtf8TextTransport(&'static str);

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl GatewayTransport for InvalidUtf8TextTransport {
    async fn send(
        &self,
        _request: GatewayRequest,
        _body_limit: GatewayResponseBodyLimit,
    ) -> Result<GatewayResponse> {
        GatewayResponse::new(
            200,
            vec![GatewayHeader::new("Content-Type", self.0)?],
            vec![b'<', 0xff, b'>'],
        )
    }
}

#[test]
fn test_gateway_rejects_non_utf8_rewritable_documents() -> Result<()> {
    let target = TargetUrl::parse("https://example.com/index.html")?.into_url();
    for content_type in [
        "text/html; charset=iso-8859-1",
        "text/css; charset=iso-8859-1",
    ] {
        let gateway = ConcurrentWebviewGateway::new(
            GatewayPrefix::new("/webview/")?,
            InvalidUtf8TextTransport(content_type),
        );
        let result =
            futures::executor::block_on(gateway.send(GatewayRequest::navigation(target.clone())));

        assert!(matches!(
            result,
            Err(WebviewError::UnrewritableTextEncoding { content_type: actual })
                if actual == normalize_media_type(content_type)
        ));
    }
    Ok(())
}

struct DocumentTransport {
    content_type: Option<&'static str>,
    body: Vec<u8>,
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl GatewayTransport for DocumentTransport {
    async fn send(
        &self,
        _request: GatewayRequest,
        _body_limit: GatewayResponseBodyLimit,
    ) -> Result<GatewayResponse> {
        let headers = self
            .content_type
            .map(|value| GatewayHeader::new("Content-Type", value))
            .transpose()?
            .into_iter()
            .collect();
        GatewayResponse::new(200, headers, self.body.clone())
    }
}

#[test]
fn test_declared_active_and_undeclared_active_navigation_documents_are_rewritten() -> Result<()> {
    let target = TargetUrl::parse("https://example.com/index")?.into_url();
    for (content_type, body) in [
        (
            Some("application/xhtml+xml"),
            br#"<html><img src="/xhtml.png"></html>"#.as_slice(),
        ),
        (
            Some("image/svg+xml"),
            br#"<svg><image href="/svg.png"/></svg>"#.as_slice(),
        ),
        (
            None,
            br#"<!-- prefix --><html><img src="/missing.png"></html>"#.as_slice(),
        ),
    ] {
        let gateway =
            ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, DocumentTransport {
                content_type,
                body: body.to_vec(),
            });

        let response =
            futures::executor::block_on(gateway.send(GatewayRequest::navigation(target.clone())))?;

        assert!(
            String::from_utf8(response.body)
                .map_err(|error| WebviewError::transport(error.to_string()))?
                .contains("/webview/"),
            "active document was not rewritten for {content_type:?}"
        );
    }
    Ok(())
}

#[test]
fn test_declared_inert_navigation_bodies_are_not_activated_by_markup_bytes() -> Result<()> {
    let target = TargetUrl::parse("https://example.com/index")?.into_url();
    for content_type in [
        "text/plain",
        "text/xml",
        "application/xml",
        "application/problem+json",
        "image/png",
    ] {
        let body = br#"<html><script>globalThis.activated = true</script></html>"#.to_vec();
        let gateway =
            ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, DocumentTransport {
                content_type: Some(content_type),
                body: body.clone(),
            });

        let response =
            futures::executor::block_on(gateway.send(GatewayRequest::navigation(target.clone())))?;

        assert_eq!(
            response.body, body,
            "declared {content_type} must stay inert"
        );
    }
    Ok(())
}

#[test]
fn test_unknown_navigation_media_type_is_rejected_instead_of_passed_through() -> Result<()> {
    let target = TargetUrl::parse("https://example.com/index")?.into_url();
    let gateway =
        ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, DocumentTransport {
            content_type: Some("application/octet-stream"),
            body: b"opaque".to_vec(),
        });

    assert!(matches!(
        futures::executor::block_on(gateway.send(GatewayRequest::navigation(target))),
        Err(WebviewError::UnsafeNavigationMediaType { content_type })
            if content_type.as_deref() == Some("application/octet-stream")
    ));
    Ok(())
}

#[test]
fn test_missing_navigation_media_type_is_rejected_when_no_active_prefix_is_proven() -> Result<()> {
    let target = TargetUrl::parse("https://example.com/index")?.into_url();
    let gateway =
        ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, DocumentTransport {
            content_type: None,
            body: b"opaque".to_vec(),
        });

    assert!(matches!(
        futures::executor::block_on(gateway.send(GatewayRequest::navigation(target))),
        Err(WebviewError::UnsafeNavigationMediaType { content_type: None })
    ));
    Ok(())
}

#[test]
fn test_invalid_tail_cannot_hide_active_document_without_content_type() -> Result<()> {
    let target = TargetUrl::parse("https://example.com/index")?.into_url();
    let gateway =
        ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, DocumentTransport {
            content_type: None,
            body: [b"<html><body>active</body></html>".as_slice(), &[0xff]].concat(),
        });

    assert!(matches!(
        futures::executor::block_on(gateway.send(GatewayRequest::navigation(target))),
        Err(WebviewError::UnrewritableTextEncoding { content_type })
            if content_type == "sniffed active document"
    ));
    Ok(())
}

#[test]
fn test_gateway_rejects_response_before_rewriting_when_body_budget_is_exceeded() -> Result<()> {
    let target = TargetUrl::parse("https://example.com/index")?.into_url();
    let gateway =
        ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, DocumentTransport {
            content_type: Some("text/html"),
            body: vec![b'x'; MAX_GATEWAY_BODY_BYTES + 1],
        });

    assert!(matches!(
        futures::executor::block_on(gateway.send(GatewayRequest::navigation(target))),
        Err(WebviewError::Transport(
            crate::error::TransportFailure::ResponseBodyTooLarge { .. }
        ))
    ));
    Ok(())
}

#[test]
fn test_gateway_rejects_oversized_request_before_transport_preparation() -> Result<()> {
    let target = TargetUrl::parse("https://example.com/upload")?.into_url();
    let gateway = ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, StaticTransport);
    let request = GatewayRequest::navigation(target).with_body(vec![0; MAX_GATEWAY_BODY_BYTES + 1]);

    assert!(matches!(
        futures::executor::block_on(gateway.send(request)),
        Err(WebviewError::Transport(
            crate::error::TransportFailure::RequestBodyTooLarge { .. }
        ))
    ));
    Ok(())
}

#[test]
fn test_gateway_rejects_response_when_rewriting_exceeds_body_budget() -> Result<()> {
    let target = TargetUrl::parse("https://example.com/index")?.into_url();
    for (content_type, body) in [
        ("text/html", "<img src=\"/x\">".repeat(200_000)),
        ("text/css", "a{background:url(/x)}".repeat(200_000)),
    ] {
        assert!(body.len() < MAX_GATEWAY_BODY_BYTES);
        let gateway =
            ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, DocumentTransport {
                content_type: Some(content_type),
                body: body.into_bytes(),
            });

        assert!(matches!(
            futures::executor::block_on(gateway.send(GatewayRequest::navigation(target.clone()))),
            Err(WebviewError::Transport(
                crate::error::TransportFailure::ResponseBodyTooLarge { .. }
            ))
        ));
    }
    Ok(())
}

fn request_bootstrap(request: &GatewayRequest) -> String {
    format!(
        "globalThis.__ringsTopLevelNavigation = {};",
        request.top_level_navigation
    )
}

#[test]
fn test_per_request_bootstrap_tracks_navigation_context() -> Result<()> {
    let gateway = ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, StaticTransport)
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
fn test_source_free_runtime_gateway_requests_are_rejected() -> Result<()> {
    let target = TargetUrl::parse("https://api.example.test/data")?.into_url();
    let gateway = ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, StaticTransport);

    let send = futures::executor::block_on(gateway.send(GatewayRequest::fetch(target, "GET")));
    assert!(matches!(
        send,
        Err(WebviewError::MissingRuntimeSourceOrigin)
    ));
    Ok(())
}

struct DomainCookieTransport;

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl GatewayTransport for DomainCookieTransport {
    async fn send(
        &self,
        _request: GatewayRequest,
        _body_limit: GatewayResponseBodyLimit,
    ) -> Result<GatewayResponse> {
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
fn test_gateway_accepts_safe_domain_cookie_without_exposing_set_cookie() -> Result<()> {
    let target = TargetUrl::parse("https://example.com/index.html")?.into_url();
    let gateway =
        ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, DomainCookieTransport);

    let response = futures::executor::block_on(gateway.send(GatewayRequest::navigation(target)))?;
    let body = String::from_utf8(response.body)
        .map_err(|error| WebviewError::transport(error.to_string()))?;

    assert!(body.contains("<p>ok</p>"));
    assert_eq!(gateway.lock_cookies()?.len(), 1);
    assert!(!response
        .headers
        .iter()
        .any(|header| header.name_eq("set-cookie")));
    Ok(())
}

struct RecordingTransport {
    requests: Mutex<Vec<GatewayRequest>>,
}

impl RecordingTransport {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl GatewayTransport for RecordingTransport {
    async fn send(
        &self,
        request: GatewayRequest,
        _body_limit: GatewayResponseBodyLimit,
    ) -> Result<GatewayResponse> {
        self.requests.test_lock()?.push(request);
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
fn test_gateway_rejects_direct_private_request_before_transport_io() -> Result<()> {
    let gateway =
        ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, RecordingTransport::new());
    let request = GatewayRequest::navigation(Url::parse("http://169.254.169.254/metadata")?);

    assert!(matches!(
        futures::executor::block_on(gateway.send(request)),
        Err(WebviewError::UnsafeTargetHost(_))
    ));
    assert!(gateway.transport.requests.test_lock()?.is_empty());
    Ok(())
}

#[test]
fn test_gateway_replaces_caller_cookie_header_with_virtual_target_cookie() -> Result<()> {
    let transport = RecordingTransport::new();
    let target = TargetUrl::parse("https://example.com/index.html")?.into_url();
    let fetch_target = TargetUrl::parse("https://example.com/api")?.into_url();
    let gateway = ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, transport);

    futures::executor::block_on(gateway.send(GatewayRequest::navigation(target)))?;
    futures::executor::block_on(
        gateway.send(
            GatewayRequest::fetch(fetch_target, "GET")
                .with_source_origin(TargetUrl::parse("https://example.com/index.html")?.into_url())
                .with_header(GatewayHeader::new("Cookie", "caller=leak")?),
        ),
    )?;

    let requests = gateway.transport.requests.test_lock()?;
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
fn test_gateway_normalizes_direct_struct_source_origin_before_transport() -> Result<()> {
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
    let gateway = ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, transport);

    futures::executor::block_on(gateway.send(request))?;

    let requests = gateway.transport.requests.test_lock()?;
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
fn test_gateway_strips_controlled_origin_headers_before_transport() -> Result<()> {
    let transport = RecordingTransport::new();
    let target = TargetUrl::parse("https://example.com/index.html")?.into_url();
    let gateway = ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, transport);

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

    let requests = gateway.transport.requests.test_lock()?;
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
        .any(|header| header.name_eq("accept") && header.value == "*/*"));
    Ok(())
}

struct CorsRecordingTransport {
    requests: Mutex<Vec<GatewayRequest>>,
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl GatewayTransport for CorsRecordingTransport {
    async fn send(
        &self,
        request: GatewayRequest,
        _body_limit: GatewayResponseBodyLimit,
    ) -> Result<GatewayResponse> {
        let is_preflight = request.method == "OPTIONS";
        self.requests.test_lock()?.push(request);
        let mut headers = vec![
            GatewayHeader::new("Access-Control-Allow-Origin", "https://app.example.test")?,
            GatewayHeader::new("Content-Type", "text/plain")?,
        ];
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
fn test_gateway_forwards_cross_origin_runtime_requests_after_virtual_cors_preflight() -> Result<()>
{
    let target = TargetUrl::parse("https://api.example.test/data")?.into_url();
    let source = TargetUrl::parse("https://app.example.test/page")?.into_url();
    let transport = CorsRecordingTransport {
        requests: Mutex::new(Vec::new()),
    };
    let gateway = ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, transport);
    let response = futures::executor::block_on(
        gateway.send(
            GatewayRequest::fetch(target, "PATCH")
                .with_source_origin(source)
                .with_header(GatewayHeader::new("X-Requested-With", "Rings")?),
        ),
    )?;

    assert_eq!(response.body, b"cors response");
    let requests = gateway.transport.requests.test_lock()?;
    assert_eq!(requests.len(), 2);
    let preflight = requests
        .first()
        .ok_or_else(|| WebviewError::transport("missing CORS preflight".to_string()))?;
    assert_eq!(preflight.method, "OPTIONS");
    assert!(preflight
        .headers
        .iter()
        .any(|header| { header.name_eq("origin") && header.value == "https://app.example.test" }));
    assert!(preflight.headers.iter().any(|header| {
        header.name_eq("access-control-request-method") && header.value == "PATCH"
    }));
    let actual = requests
        .get(1)
        .ok_or_else(|| WebviewError::transport("missing CORS runtime request".to_string()))?;
    assert_eq!(actual.method, "PATCH");
    assert!(actual
        .headers
        .iter()
        .any(|header| { header.name_eq("origin") && header.value == "https://app.example.test" }));
    Ok(())
}

struct ParityTransport {
    requests: Mutex<Vec<GatewayRequest>>,
}

impl ParityTransport {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl GatewayTransport for ParityTransport {
    async fn send(
        &self,
        request: GatewayRequest,
        _body_limit: GatewayResponseBodyLimit,
    ) -> Result<GatewayResponse> {
        let is_preflight = request.method == "OPTIONS";
        self.requests.test_lock()?.push(request);
        let mut headers = vec![
            GatewayHeader::new("Access-Control-Allow-Origin", "https://app.example.test")?,
            GatewayHeader::new("Content-Type", "text/plain")?,
        ];
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
fn test_concurrent_gateway_applies_cookie_cors_and_response_policy() -> Result<()> {
    let gateway =
        ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, ParityTransport::new());
    for request in parity_request_sequence()? {
        futures::executor::block_on(gateway.send(request))?;
    }

    assert_eq!(gateway.lock_cookies()?.len(), 1);
    let requests = gateway.transport.requests.test_lock()?;
    let final_request = requests.last().ok_or_else(|| {
        WebviewError::transport("gateway transport did not receive final request".to_string())
    })?;
    assert!(final_request
        .headers
        .iter()
        .any(|header| header.name_eq("cookie") && header.value == "sid=one"));
    Ok(())
}

struct DelayedCookieTransport {
    started: mpsc::UnboundedSender<String>,
    delayed_path: String,
    release_delayed_request: Mutex<Option<oneshot::Receiver<()>>>,
    requests: Mutex<Vec<GatewayRequest>>,
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl GatewayTransport for DelayedCookieTransport {
    async fn send(
        &self,
        request: GatewayRequest,
        _body_limit: GatewayResponseBodyLimit,
    ) -> Result<GatewayResponse> {
        let path = request.target.path().to_string();
        self.requests.test_lock()?.push(request);
        let _ = self.started.unbounded_send(path.clone());
        if path == self.delayed_path {
            let receiver = self
                .release_delayed_request
                .test_lock()?
                .take()
                .ok_or_else(|| {
                    WebviewError::transport("delayed request was released twice".to_string())
                })?;
            receiver.await.map_err(|_| {
                WebviewError::transport("delayed request release channel was dropped".to_string())
            })?;
        }
        GatewayResponse::new(
            200,
            vec![
                GatewayHeader::new("content-type", "text/plain")?,
                GatewayHeader::new("set-cookie", format!("sid={}; Path=/", &path[1..]))?,
            ],
            path.into_bytes(),
        )
    }
}

#[test]
fn test_concurrent_gateway_cookie_commits_follow_response_order_and_source_visibility() -> Result<()>
{
    let (started_sender, mut started_receiver) = mpsc::unbounded();
    let (release_slow_sender, release_slow_receiver) = oneshot::channel();
    let gateway = Rc::new(ConcurrentWebviewGateway::new(
        GatewayPrefix::new("/webview/")?,
        DelayedCookieTransport {
            started: started_sender,
            delayed_path: "/slow".to_string(),
            release_delayed_request: Mutex::new(Some(release_slow_receiver)),
            requests: Mutex::new(Vec::new()),
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
            let _ =
                slow_result_sender.send(slow_gateway.send(GatewayRequest::navigation(slow)).await);
        })
        .map_err(|error| WebviewError::transport(error.to_string()))?;
    assert_eq!(
        pool.run_until(started_receiver.next()),
        Some("/slow".to_string())
    );

    let fast_gateway = Rc::clone(&gateway);
    let fast_request = fast.clone();
    spawner
        .spawn_local(async move {
            let _ = fast_result_sender.send(
                fast_gateway
                    .send(GatewayRequest::subresource(fast_request))
                    .await,
            );
        })
        .map_err(|error| WebviewError::transport(error.to_string()))?;
    let fast_response = pool
        .run_until(fast_result_receiver)
        .map_err(|_| WebviewError::transport("fast resource task was dropped".to_string()))??;
    assert_eq!(fast_response.body, b"/fast");
    assert!(gateway
        .transport
        .requests
        .test_lock()?
        .iter()
        .all(|request| {
            !request
                .headers
                .iter()
                .any(|header| header.name_eq("cookie"))
        }));

    // The fast response has committed while the first request remains in flight, so a
    // same-site intermediate request must observe it before the slow response can overwrite
    // it. This makes response-order visibility observable, rather than inferring it from the
    // final jar alone.
    let same_site_read = TargetUrl::parse("https://example.test/read-same-site")?.into_url();
    let same_site_response = pool.run_until(
        gateway.send(
            GatewayRequest::subresource(same_site_read.clone())
                .with_source_origin(Url::parse("https://example.test/page")?),
        ),
    )?;
    assert_eq!(same_site_response.body, b"/read-same-site");
    let same_site_request = gateway
        .transport
        .requests
        .test_lock()?
        .last()
        .cloned()
        .ok_or_else(|| WebviewError::transport("missing same-site read".to_string()))?;
    assert!(same_site_request
        .headers
        .iter()
        .any(|header| header.name_eq("cookie") && header.value == "sid=fast"));

    // A cross-site subresource shares the target host but not the source origin. Its Lax
    // cookie view must remain empty; otherwise a request prepared from another controlled
    // page could leak the intermediate session.
    let cross_site_read = TargetUrl::parse("https://example.test/read-cross-site")?.into_url();
    let cross_site_response = pool.run_until(
        gateway.send(
            GatewayRequest::subresource(cross_site_read)
                .with_source_origin(Url::parse("https://attacker.example/page")?),
        ),
    )?;
    assert_eq!(cross_site_response.body, b"/read-cross-site");
    let cross_site_request = gateway
        .transport
        .requests
        .test_lock()?
        .last()
        .cloned()
        .ok_or_else(|| WebviewError::transport("missing cross-site read".to_string()))?;
    assert!(!cross_site_request
        .headers
        .iter()
        .any(|header| header.name_eq("cookie")));

    release_slow_sender.send(()).map_err(|_| {
        WebviewError::transport("slow resource task stopped waiting unexpectedly".to_string())
    })?;
    let slow_response = pool
        .run_until(slow_result_receiver)
        .map_err(|_| WebviewError::transport("slow resource task was dropped".to_string()))??;
    assert_eq!(slow_response.body, b"/slow");
    assert_eq!(
        gateway.lock_cookies()?.cookie_header(&fast).as_deref(),
        Some("sid=slow")
    );

    let after = TargetUrl::parse("https://example.test/after")?.into_url();
    let after_response = pool.run_until(gateway.send(GatewayRequest::navigation(after)))?;
    assert_eq!(after_response.body, b"/after");
    let after_request = gateway
        .transport
        .requests
        .test_lock()?
        .last()
        .cloned()
        .ok_or_else(|| WebviewError::transport("missing post-commit request".to_string()))?;
    assert!(after_request
        .headers
        .iter()
        .any(|header| header.name_eq("cookie") && header.value == "sid=slow"));
    Ok(())
}

#[test]
fn test_concurrent_gateway_cookie_commits_in_mirror_response_order() -> Result<()> {
    let (started_sender, mut started_receiver) = mpsc::unbounded();
    let (release_fast_sender, release_fast_receiver) = oneshot::channel();
    let gateway = Rc::new(ConcurrentWebviewGateway::new(
        GatewayPrefix::new("/webview/")?,
        DelayedCookieTransport {
            started: started_sender,
            delayed_path: "/fast".to_string(),
            release_delayed_request: Mutex::new(Some(release_fast_receiver)),
            requests: Mutex::new(Vec::new()),
        },
    ));
    let slow = TargetUrl::parse("https://example.test/slow")?.into_url();
    let fast = TargetUrl::parse("https://example.test/fast")?.into_url();
    let same_site_source = Url::parse("https://example.test/page")?;
    let (fast_result_sender, fast_result_receiver) = oneshot::channel();
    let (slow_result_sender, slow_result_receiver) = oneshot::channel();
    let mut pool = LocalPool::new();
    let spawner = pool.spawner();

    let fast_gateway = Rc::clone(&gateway);
    spawner
        .spawn_local(async move {
            let _ =
                fast_result_sender.send(fast_gateway.send(GatewayRequest::navigation(fast)).await);
        })
        .map_err(|error| WebviewError::transport(error.to_string()))?;
    assert_eq!(
        pool.run_until(started_receiver.next()),
        Some("/fast".to_string())
    );

    let slow_gateway = Rc::clone(&gateway);
    let slow_request = slow.clone();
    let slow_source = same_site_source.clone();
    spawner
        .spawn_local(async move {
            let _ = slow_result_sender.send(
                slow_gateway
                    .send(GatewayRequest::subresource(slow_request).with_source_origin(slow_source))
                    .await,
            );
        })
        .map_err(|error| WebviewError::transport(error.to_string()))?;
    let slow_response = pool
        .run_until(slow_result_receiver)
        .map_err(|_| WebviewError::transport("slow resource task was dropped".to_string()))??;
    assert_eq!(slow_response.body, b"/slow");

    let intermediate = TargetUrl::parse("https://example.test/intermediate")?.into_url();
    let intermediate_response = pool.run_until(
        gateway.send(
            GatewayRequest::subresource(intermediate)
                .with_source_origin(Url::parse(same_site_source.as_str())?),
        ),
    )?;
    assert_eq!(intermediate_response.body, b"/intermediate");
    let intermediate_request = gateway
        .transport
        .requests
        .test_lock()?
        .last()
        .cloned()
        .ok_or_else(|| WebviewError::transport("missing intermediate read".to_string()))?;
    assert!(intermediate_request
        .headers
        .iter()
        .any(|header| header.name_eq("cookie") && header.value == "sid=slow"));

    release_fast_sender.send(()).map_err(|_| {
        WebviewError::transport("fast resource task stopped waiting unexpectedly".to_string())
    })?;
    let fast_response = pool
        .run_until(fast_result_receiver)
        .map_err(|_| WebviewError::transport("fast resource task was dropped".to_string()))??;
    assert_eq!(fast_response.body, b"/fast");
    assert_eq!(
        gateway.lock_cookies()?.cookie_header(&slow).as_deref(),
        Some("sid=fast")
    );
    Ok(())
}
