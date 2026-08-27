use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::task::Context;
use std::task::Poll;

use futures::task::noop_waker;
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
    async fn send(
        &self,
        request: GatewayRequest,
        _body_limit: rings_webview::GatewayResponseBodyLimit,
    ) -> WebviewResult<GatewayResponse> {
        self.requests.borrow_mut().push(request);
        let request = self
            .requests
            .borrow()
            .last()
            .cloned()
            .ok_or_else(|| WebviewError::transport("missing fixture request".to_string()))?;
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
            .with_request_bootstrap(web_shell_bootstrap),
        limiter: GatewayRequestLimiter::new(
            MAX_CONCURRENT_GATEWAY_REQUESTS,
            MAX_QUEUED_GATEWAY_REQUESTS,
        ),
    };
    Ok((host, requests))
}

#[wasm_bindgen_test]
fn test_extension_bootstrap_omits_web_overlay_and_preserves_worker_bridge() -> WebviewResult<()> {
    let request = GatewayRequest::navigation(TargetUrl::parse("https://example.test/")?.into_url());

    let bootstrap = extension_webview_bootstrap(&request);

    assert!(!bootstrap.contains("data-rings-webview-overlay-loader"));
    assert!(bootstrap.contains("\"blockWorkers\":false"));
    assert!(bootstrap.contains("\"delegateNavigation\":true"));
    Ok(())
}

#[wasm_bindgen_test]
fn test_webview_onion_settings_requires_explicit_short_path_opt_in() {
    let settings = WebviewOnionSettings::default();
    assert!(!settings.options().allow_short_paths);

    settings.set_allow_short_paths(true);

    assert!(settings.options().allow_short_paths);
}

#[wasm_bindgen_test]
fn test_browser_gateway_request_ids_are_positive_safe_integers() {
    assert_eq!(browser_request_id(&JsValue::from_f64(7.0)), Ok(7));
    assert!(browser_request_id(&JsValue::from_f64(0.0)).is_err());
    assert!(browser_request_id(&JsValue::from_f64(1.5)).is_err());
    assert!(browser_request_id(&JsValue::from_str("7")).is_err());
}

#[wasm_bindgen_test]
fn test_cancelled_woken_gateway_waiter_returns_its_transferred_permit() {
    let limiter = GatewayRequestLimiter::new(1, 1);
    let active = futures::executor::block_on(limiter.acquire());
    assert!(active.is_ok(), "the first permit must be admitted");
    let Some(active) = active.ok() else {
        return;
    };
    let waiting_limiter = limiter.clone();
    let mut waiting = Box::pin(waiting_limiter.acquire());
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);

    assert!(matches!(waiting.as_mut().poll(&mut context), Poll::Pending));
    drop(active);
    drop(waiting);

    let state = limiter.state.borrow();
    assert_eq!(state.available, 1);
    assert!(state.waiters.is_empty());
}

#[wasm_bindgen_test]
fn test_gateway_waiter_queue_fails_closed_at_its_bound() {
    let limiter = GatewayRequestLimiter::new(1, 1);
    let active = futures::executor::block_on(limiter.acquire());
    assert!(active.is_ok(), "the active permit must be admitted");
    let Some(active) = active.ok() else {
        return;
    };
    let waiting_limiter = limiter.clone();
    let mut waiting = Box::pin(waiting_limiter.acquire());
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(waiting.as_mut().poll(&mut context), Poll::Pending));

    assert!(matches!(
        futures::executor::block_on(limiter.acquire()),
        Err(GatewayRequestAdmissionError::QueueFull)
    ));
    assert_eq!(limiter.state.borrow().waiters.len(), 1);

    drop(waiting);
    drop(active);
    let state = limiter.state.borrow();
    assert_eq!(state.available, 1);
    assert!(state.waiters.is_empty());
}

#[wasm_bindgen_test]
fn test_browser_request_allows_source_free_subresources() -> WebviewResult<()> {
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
fn test_host_redirects_then_serves_a_gateway_document_through_its_transport() -> WebviewResult<()> {
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
        return Err(WebviewError::transport(
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
        return Err(WebviewError::transport(
            "gateway document was not served".to_string(),
        ));
    };
    let body = String::from_utf8(response.body)
        .map_err(|error| WebviewError::transport(error.to_string()))?;

    assert!(body.contains("data-rings-webview-bootstrap"));
    assert!(body.contains("data-rings-webview-overlay-loader"));
    assert!(body.contains("/webview/https%3A%2F%2Fexample%2Etest%2Fasset%2Epng"));
    assert_eq!(requests.borrow().len(), 1);
    let sent_request = requests
        .borrow()
        .first()
        .cloned()
        .ok_or_else(|| WebviewError::transport("missing gateway request".to_string()))?;
    assert_eq!(sent_request.target.as_str(), target.as_url().as_str());
    assert_eq!(sent_request.method, "POST");
    assert_eq!(sent_request.body, vec![0x00, 0xff]);
    assert_eq!(sent_request.headers, vec![
        GatewayHeader::new("Accept", "*/*")?,
        GatewayHeader::new("Accept-Encoding", "identity")?,
    ]);
    Ok(())
}

#[wasm_bindgen_test]
fn test_iframe_navigation_gets_bootstrap_without_webview_overlay() -> WebviewResult<()> {
    let (host, requests) = fixture_host()?;
    let target = TargetUrl::parse("https://frame.example.test/nested.html")?;
    let gateway_target = TargetUrl::parse(host.policy.gateway_url(target.as_url())?.as_str())?;

    let response = futures::executor::block_on(
        host.handle(
            WebviewHostRequest::navigation_with_payload(
                gateway_target,
                "GET",
                Vec::new(),
                Vec::new(),
            )
            .with_top_level_navigation(false),
        ),
    )?;
    let WebviewHostOutcome::Response(response) = response else {
        return Err(WebviewError::transport(
            "iframe gateway document was not served".to_string(),
        ));
    };
    let body = String::from_utf8(response.body)
        .map_err(|error| WebviewError::transport(error.to_string()))?;

    assert!(body.contains("data-rings-webview-bootstrap"));
    assert!(!body.contains("data-rings-webview-overlay-loader"));
    assert!(body.contains("/webview/https%3A%2F%2Fframe%2Eexample%2Etest%2Fasset%2Epng"));
    assert_eq!(requests.borrow().len(), 1);
    let sent_request = requests
        .borrow()
        .first()
        .cloned()
        .ok_or_else(|| WebviewError::transport("missing iframe gateway request".to_string()))?;
    assert_eq!(sent_request.target.as_str(), target.as_url().as_str());
    assert!(!sent_request.top_level_navigation);
    Ok(())
}

#[wasm_bindgen_test]
fn test_host_serves_cross_target_runtime_reads_when_upstream_allows_cors() -> WebviewResult<()> {
    let (host, requests) = fixture_host()?;
    let source = TargetUrl::parse("https://app.example.test/index.html?q=1#section")?;
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
        return Err(WebviewError::transport(format!(
            "runtime fetch did not serve through gateway: {outcome:?}"
        )));
    };
    let request = requests
        .borrow()
        .first()
        .cloned()
        .ok_or_else(|| WebviewError::transport("missing cross-origin request".to_string()))?;
    assert_eq!(
        request.source_origin.as_ref().map(Url::as_str),
        Some("https://app.example.test/")
    );
    Ok(())
}

#[wasm_bindgen_test]
fn test_browser_cookie_expiry_sequence_uses_explicit_clock() -> WebviewResult<()> {
    let mut jar = CookieJar::new();
    let origin = Url::parse("https://example.test/app/index.html")?;
    let target = Url::parse("https://example.test/app/page")?;
    let now = 1_000;

    jar.store_set_cookie_at(&origin, "sid=one; Path=/app; Max-Age=2", now)?;
    assert_eq!(
        jar.cookie_header_at(&target, now + 1_999).as_deref(),
        Some("sid=one")
    );
    assert_eq!(jar.len_at(now + 1_999), 1);
    assert_eq!(jar.cookie_header_at(&target, now + 2_000), None);
    assert_eq!(jar.len_at(now + 2_000), 0);

    jar.store_set_cookie_at(&origin, "sid=old; Path=/app", now)?;
    jar.store_set_cookie_at(&origin, "sid=new; Path=/app", now)?;
    assert_eq!(
        jar.cookie_header_at(&target, now).as_deref(),
        Some("sid=new")
    );

    jar.store_set_cookie_at(&origin, "sid=gone; Path=/app; Max-Age=0", now)?;
    assert_eq!(jar.cookie_header_at(&target, now), None);
    assert!(jar.is_empty_at(now));

    jar.store_set_cookie_at(&origin, "sid=one; Path=/app", now)?;
    jar.store_set_cookie_at(
        &origin,
        "sid=gone; Path=/app; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
        now,
    )?;
    assert_eq!(jar.cookie_header_at(&target, now), None);
    assert!(jar.is_empty_at(now));
    Ok(())
}

#[wasm_bindgen_test]
fn test_onion_route_unavailable_is_reported_without_wasm_stack() -> WebviewResult<()> {
    let response = browser_transport_failure(onion_gateway_failure(
        rings_node::error::Error::OnionRouteError(rings_node::onion::OnionRouteError::NoLiveExit {
            service: "https".to_string(),
        })
        .into(),
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
        "gateway transport failed: Onion route error: no live onion exit offers service \"https\""
    );
    assert!(!error.contains("wasm-function"));
    Ok(())
}

#[wasm_bindgen_test]
fn test_http_origin_predicate_excludes_extension_and_file_hosts() {
    assert!(is_http_origin("https://frontend.rings.test"));
    assert!(is_http_origin("http://127.0.0.1:8080"));
    assert!(!is_http_origin("chrome-extension://rings"));
    assert!(!is_http_origin("null"));
}
