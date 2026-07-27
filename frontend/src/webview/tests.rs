use std::cell::RefCell;
use std::rc::Rc;

use rings_webview::CookieJar;
use wasm_bindgen_test::wasm_bindgen_test;

use super::*;

type RecordedRequests = Rc<RefCell<Vec<GatewayRequest>>>;
type FixtureHost = WebviewGatewayHost<FixtureTransport>;

struct FixtureTransport {
    requests: RecordedRequests,
}

impl FixtureTransport {
    fn new(requests: RecordedRequests) -> Self {
        Self { requests }
    }
}

#[async_trait(?Send)]
impl GatewayTransport for FixtureTransport {
    async fn send(&self, request: GatewayRequest) -> WebviewResult<GatewayResponse> {
        self.requests.borrow_mut().push(request);
        let request = self
            .requests
            .borrow()
            .last()
            .cloned()
            .ok_or_else(|| WebviewError::Transport("missing fixture request".to_string()))?;
        let mut headers = vec![GatewayHeader::new("content-type", "text/html")?];
        if let Some(source) = request.source_origin {
            headers.push(GatewayHeader::new(
                "access-control-allow-origin",
                source.origin().ascii_serialization(),
            )?);
        }
        GatewayResponse::new(200, headers, b"<img src=\"/asset.png\">".to_vec())
    }
}

fn fixture_host() -> WebviewResult<(FixtureHost, RecordedRequests)> {
    let prefix = GatewayPrefix::new(GATEWAY_PREFIX)?;
    let policy = GatewayRoutePolicy::new(
        TargetUrl::parse("https://frontend.rings.test/")?.into_url(),
        prefix.clone(),
    )?;
    let requests = Rc::new(RefCell::new(Vec::new()));
    let host = WebviewGatewayHost {
        policy,
        gateway: ConcurrentWebviewGateway::new(prefix, FixtureTransport::new(requests.clone()))
            .with_target_bootstrap(webview_bootstrap),
        limiter: GatewayRequestLimiter::new(MAX_CONCURRENT_GATEWAY_REQUESTS),
    };
    Ok((host, requests))
}

#[wasm_bindgen_test]
fn browser_request_allows_source_free_subresources() -> WebviewResult<()> {
    let value = Object::new();
    crate::browser_api::js_set(
        &value,
        "requested",
        &JsValue::from_str("https://static.example.test/site.css"),
    )
    .map_err(WebviewError::Browser)?;
    crate::browser_api::js_set(&value, "method", &JsValue::from_str("GET"))
        .map_err(WebviewError::Browser)?;
    crate::browser_api::js_set(&value, "kind", &JsValue::from_str("subresource"))
        .map_err(WebviewError::Browser)?;

    let request = browser_host_request(&value.into()).map_err(WebviewError::Browser)?;

    assert_eq!(request.kind, GatewayRequestKind::Subresource);
    assert!(request.source_target.is_none());
    Ok(())
}

#[wasm_bindgen_test]
fn host_redirects_then_serves_a_gateway_document_through_its_transport() -> WebviewResult<()> {
    let (host, requests) = fixture_host()?;
    let target = TargetUrl::parse("https://example.test/docs/index.html")?;
    let headers = vec![GatewayHeader::new("accept", "text/html")?];
    let body = vec![0x00, 0xff];
    let redirect =
        futures::executor::block_on(host.handle(WebviewHostRequest::navigation_with_payload(
            target.clone(),
            "POST",
            headers.clone(),
            body.clone(),
        )))?;
    let WebviewHostOutcome::Redirect(gateway_url) = redirect else {
        return Err(WebviewError::Transport(
            "navigation did not redirect to gateway".to_string(),
        ));
    };

    let response =
        futures::executor::block_on(host.handle(WebviewHostRequest::navigation_with_payload(
            TargetUrl::parse(gateway_url.as_str())?,
            "POST",
            headers,
            body,
        )))?;
    let WebviewHostOutcome::Response(response) = response else {
        return Err(WebviewError::Transport(
            "gateway document was not served".to_string(),
        ));
    };
    let body = String::from_utf8(response.body)
        .map_err(|error| WebviewError::Transport(error.to_string()))?;

    assert!(body.contains("data-rings-webview-bootstrap"));
    assert!(body.contains("/assets/webview-overlay.js"));
    assert!(body.contains("/webview/https%3A%2F%2Fexample%2Etest%2Fasset%2Epng"));
    assert_eq!(requests.borrow().len(), 1);
    let sent_request = requests
        .borrow()
        .first()
        .cloned()
        .ok_or_else(|| WebviewError::Transport("missing gateway request".to_string()))?;
    assert_eq!(sent_request.target.as_str(), target.as_url().as_str());
    assert_eq!(sent_request.method, "POST");
    assert_eq!(sent_request.body, vec![0x00, 0xff]);
    assert_eq!(sent_request.headers, vec![
        GatewayHeader::new("accept", "text/html")?,
        GatewayHeader::new("Accept-Encoding", "identity")?,
    ]);
    Ok(())
}

#[wasm_bindgen_test]
fn host_serves_cross_target_runtime_reads_when_upstream_allows_cors() -> WebviewResult<()> {
    let (host, requests) = fixture_host()?;
    let source = TargetUrl::parse("https://app.example.test/index.html")?;
    let target = TargetUrl::parse("https://bank.example.test/account")?;
    let gateway_target = TargetUrl::parse(host.policy.gateway_url(target.as_url())?.as_str())?;

    let outcome = futures::executor::block_on(host.handle(WebviewHostRequest::fetch(
        gateway_target,
        source.clone(),
        "GET",
        Vec::new(),
        Vec::new(),
    )))?;

    let WebviewHostOutcome::Response(_) = outcome else {
        return Err(WebviewError::Transport(format!(
            "runtime fetch did not serve through gateway: {outcome:?}"
        )));
    };
    let request = requests
        .borrow()
        .first()
        .cloned()
        .ok_or_else(|| WebviewError::Transport("missing cross-origin request".to_string()))?;
    assert_eq!(request.source_origin.as_ref(), Some(source.as_url()));
    Ok(())
}

#[wasm_bindgen_test]
fn browser_cookie_expiry_uses_browser_safe_clock() -> WebviewResult<()> {
    let mut jar = CookieJar::new();
    let origin = Url::parse("https://example.test/app/index.html")?;
    let target = Url::parse("https://example.test/app/page")?;

    jar.store_set_cookie(&origin, "sid=one; Path=/app; Max-Age=60")?;
    assert_eq!(jar.cookie_header(&target).as_deref(), Some("sid=one"));

    jar.store_set_cookie(&origin, "sid=gone; Path=/app; Max-Age=0")?;
    assert_eq!(jar.cookie_header(&target), None);
    assert!(jar.is_empty());

    jar.store_set_cookie(&origin, "sid=one; Path=/app")?;
    jar.store_set_cookie(
        &origin,
        "sid=gone; Path=/app; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
    )?;
    assert_eq!(jar.cookie_header(&target), None);
    assert!(jar.is_empty());
    Ok(())
}

#[wasm_bindgen_test]
fn onion_route_unavailable_is_reported_without_wasm_stack() -> WebviewResult<()> {
    let response = browser_transport_failure(onion_gateway_failure(
        onion::OnionProxyError::classified(
        "onion proxy request failed: Onion route error: no live onion exit offers service \"https\""
            .to_string(),
        ),
    ));

    let status = crate::browser_api::js_prop(&response, "status")
        .map_err(WebviewError::Browser)?
        .as_f64()
        .ok_or_else(|| WebviewError::Browser("failure status was not numeric".to_string()))?;
    let error =
        crate::browser_api::js_string_field(&response, "error").map_err(WebviewError::Browser)?;
    let code = crate::browser_api::js_string_field(&response, "errorCode")
        .map_err(WebviewError::Browser)?;
    let summary = crate::browser_api::js_string_field(&response, "errorSummary")
        .map_err(WebviewError::Browser)?;

    assert_eq!(status, 503.0);
    assert_eq!(code, "onion_exit_unavailable");
    assert_eq!(summary, "No live HTTPS onion exit is available.");
    assert_eq!(
        error,
        "gateway transport failed: onion proxy request failed: Onion route error: no live onion exit offers service \"https\""
    );
    assert!(!error.contains("wasm-function"));
    Ok(())
}

#[wasm_bindgen_test]
fn http_origin_predicate_excludes_extension_and_file_hosts() {
    assert!(is_http_origin("https://frontend.rings.test"));
    assert!(is_http_origin("http://127.0.0.1:8080"));
    assert!(!is_http_origin("chrome-extension://rings"));
    assert!(!is_http_origin("null"));
}
