//! End-to-end policy flow coverage for the webview gateway.
#![cfg(feature = "browser")]

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use rings_webview::browser::bootstrap_script;
use rings_webview::browser::runtime_gateway_url;
use rings_webview::browser::BOOTSTRAP_MARKER;
use rings_webview::GatewayHeader;
use rings_webview::GatewayPrefix;
use rings_webview::GatewayRequest;
use rings_webview::GatewayRequestKind;
use rings_webview::GatewayResponse;
use rings_webview::GatewayTransport;
use rings_webview::Result;
use rings_webview::TargetUrl;
use rings_webview::WebviewError;
use rings_webview::WebviewGateway;
use rings_webview::WebviewRenderer;
use url::Url;

#[derive(Clone, Default)]
struct FixtureLog {
    requests: Arc<Mutex<Vec<GatewayRequest>>>,
}

impl FixtureLog {
    fn push(&self, request: GatewayRequest) -> Result<()> {
        self.requests
            .lock()
            .map_err(|error| WebviewError::transport(format!("fixture log poisoned: {error}")))?
            .push(request);
        Ok(())
    }

    fn requests(&self) -> Result<Vec<GatewayRequest>> {
        self.requests
            .lock()
            .map(|requests| requests.clone())
            .map_err(|error| WebviewError::transport(format!("fixture log poisoned: {error}")))
    }
}

struct FixtureTransport {
    log: FixtureLog,
}

impl FixtureTransport {
    fn new(log: FixtureLog) -> Self {
        Self { log }
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl GatewayTransport for FixtureTransport {
    async fn send(
        &self,
        request: GatewayRequest,
        _body_limit: rings_webview::GatewayResponseBodyLimit,
    ) -> Result<GatewayResponse> {
        let target = request.target.clone();
        self.log.push(request)?;

        match target.as_str() {
            "https://example.test/docs/index.html" => response(
                200,
                "text/html; charset=utf-8",
                vec![
                    GatewayHeader::new("Set-Cookie", "sid=fixture; Path=/; Secure")?,
                    GatewayHeader::new("Content-Security-Policy", "default-src 'none'")?,
                    GatewayHeader::new("X-Frame-Options", "DENY")?,
                ],
                br#"<!doctype html>
<html>
  <head>
    <base href="/assets/">
    <style>.inline { background: url(inline-bg.png); }</style>
    <link rel="stylesheet" href="site.css">
    <script src="app.js"></script>
  </head>
  <body>
    <a href="../next">next</a>
    <img srcset="thumb.png 1x, /hero.png 2x">
    <form action="forms/submit"></form>
  </body>
</html>"#
                    .to_vec(),
            ),
            "https://example.test/assets/site.css" => response(
                200,
                "text/css; charset=utf-8",
                Vec::new(),
                br#"@import "theme.css"; body { background: url('/img/bg.png'); }"#.to_vec(),
            ),
            "https://example.test/api/data" => response(
                200,
                "application/json",
                Vec::new(),
                br#"{"ok":true}"#.to_vec(),
            ),
            "https://example.test/docs/forms/submit" => {
                response(204, "text/plain", Vec::new(), Vec::new())
            }
            "https://example.test/assets/forms/submit" => {
                response(204, "text/plain", Vec::new(), Vec::new())
            }
            "https://example.test/redirect" => response(
                302,
                "text/html",
                vec![GatewayHeader::new("Location", "/login")?],
                Vec::new(),
            ),
            other => Err(WebviewError::transport(format!(
                "unexpected fixture request {other}"
            ))),
        }
    }
}

#[test]
fn webview_gateway_renders_and_routes_page_flow() -> Result<()> {
    let log = FixtureLog::default();
    let prefix = GatewayPrefix::new("/webview/")?;
    let target = TargetUrl::parse("https://example.test/docs/index.html")?;
    let bootstrap = bootstrap_script(prefix.as_str(), target.as_url());
    let gateway = WebviewGateway::new(prefix.clone(), FixtureTransport::new(log.clone()))
        .with_bootstrap_script(bootstrap);
    let mut renderer = WebviewRenderer::new(gateway);

    let page = futures::executor::block_on(renderer.render(target.clone()))?;

    assert_eq!(page.status, 200);
    assert_eq!(page.gateway_url, prefix.encode(target.as_url()));
    assert_contains(page.html(), "data-rings-webview-bootstrap");
    assert_contains(page.html(), BOOTSTRAP_MARKER);
    assert_contains(page.html(), "targetBase");
    assert_contains(page.html(), target.as_url().as_str());
    assert_absent(page.html(), "href=\"/assets/site.css\"");
    assert_absent(page.html(), "src=\"app.js\"");
    assert_absent(page.html(), "href=\"../next\"");
    assert_absent(page.html(), "action=\"forms/submit\"");
    assert_absent(page.html(), "url(inline-bg.png)");
    assert_header_absent(&page.headers, "set-cookie");
    assert!(page.headers.iter().any(|header| {
        header.name_eq("content-security-policy") && header.value.contains("connect-src 'self'")
    }));
    assert_header_absent(&page.headers, "x-frame-options");

    let stylesheet = Url::parse("https://example.test/assets/site.css")?;
    let script = Url::parse("https://example.test/assets/app.js")?;
    let next = Url::parse("https://example.test/next")?;
    let thumb = Url::parse("https://example.test/assets/thumb.png")?;
    let hero = Url::parse("https://example.test/hero.png")?;
    let submit = Url::parse("https://example.test/assets/forms/submit")?;
    let inline_bg = Url::parse("https://example.test/assets/inline-bg.png")?;
    let base = Url::parse("https://example.test/assets/")?;
    for expected in [
        &stylesheet,
        &script,
        &next,
        &thumb,
        &hero,
        &submit,
        &inline_bg,
        &base,
    ] {
        assert_contains(page.html(), prefix.encode(expected).as_str());
    }

    let css_path = prefix.encode(&stylesheet);
    let css_response = futures::executor::block_on(
        renderer
            .gateway_mut()
            .send_gateway_path(&css_path, GatewayRequestKind::Subresource),
    )?;
    let css = utf8_body(css_response)?;
    assert_contains(
        &css,
        prefix
            .encode(&Url::parse("https://example.test/assets/theme.css")?)
            .as_str(),
    );
    assert_contains(
        &css,
        prefix
            .encode(&Url::parse("https://example.test/img/bg.png")?)
            .as_str(),
    );

    let fetch_path = required_runtime_gateway_url(&prefix, target.as_url(), "/api/data")?;
    assert!(fetch_path.starts_with(prefix.as_str()));
    assert!(!fetch_path.starts_with("https://"));
    let fetch_target = prefix.decode_path(&fetch_path)?.into_url();
    let fetch_response = futures::executor::block_on(renderer.gateway_mut().send(
        GatewayRequest::fetch(fetch_target, "GET").with_source_origin(target.as_url().clone()),
    ))?;
    assert_eq!(utf8_body(fetch_response)?, r#"{"ok":true}"#);

    let runtime_base = Url::parse("https://example.test/assets/")?;
    let xhr_path = required_runtime_gateway_url(&prefix, &runtime_base, "forms/submit")?;
    let xhr_target = prefix.decode_path(&xhr_path)?.into_url();
    let xhr_request = GatewayRequest::xhr(xhr_target, "POST")
        .with_source_origin(target.as_url().clone())
        .with_body(b"name=value".to_vec());
    let xhr_response = futures::executor::block_on(renderer.gateway_mut().send(xhr_request))?;
    assert_eq!(xhr_response.status, 204);

    let redirect_path = prefix.encode(&Url::parse("https://example.test/redirect")?);
    let redirect_response = futures::executor::block_on(
        renderer
            .gateway_mut()
            .send_gateway_path(&redirect_path, GatewayRequestKind::Navigation),
    )?;
    assert_eq!(redirect_response.status, 302);
    let location = header_value(&redirect_response.headers, "location")
        .ok_or_else(|| WebviewError::Header("missing redirect location".to_string()))?;
    assert_eq!(
        prefix.decode_path(location)?.as_url().as_str(),
        "https://example.test/login"
    );

    let requests = log.requests()?;
    assert!(requests.iter().all(|request| {
        matches!(request.target.scheme(), "http" | "https")
            && !request.target.as_str().starts_with(prefix.as_str())
    }));
    assert!(requests.iter().any(|request| {
        request.kind == GatewayRequestKind::Subresource
            && request.target.as_str() == "https://example.test/assets/site.css"
    }));
    assert!(requests.iter().any(|request| {
        request.kind == GatewayRequestKind::Fetch
            && request.target.as_str() == "https://example.test/api/data"
            && header_value(&request.headers, "cookie") == Some("sid=fixture")
    }));
    assert!(requests.iter().any(|request| {
        request.kind == GatewayRequestKind::Xhr
            && request.method == "POST"
            && request.target.as_str() == "https://example.test/assets/forms/submit"
            && request.body.as_slice() == b"name=value"
            && header_value(&request.headers, "cookie") == Some("sid=fixture")
    }));

    Ok(())
}

fn response(
    status: u16,
    content_type: &str,
    extra_headers: Vec<GatewayHeader>,
    body: Vec<u8>,
) -> Result<GatewayResponse> {
    let mut headers = vec![GatewayHeader::new("Content-Type", content_type)?];
    headers.extend(extra_headers);
    GatewayResponse::new(status, headers, body)
}

fn required_runtime_gateway_url(
    prefix: &GatewayPrefix,
    document_url: &Url,
    input: &str,
) -> Result<String> {
    runtime_gateway_url(prefix, document_url, input)?
        .ok_or_else(|| WebviewError::InvalidGatewayUrl(input.to_string()))
}

fn utf8_body(response: GatewayResponse) -> Result<String> {
    String::from_utf8(response.body).map_err(|error| WebviewError::Render(error.to_string()))
}

fn header_value<'a>(headers: &'a [GatewayHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|header| header.name_eq(name))
        .map(|header| header.value.as_str())
}

fn assert_header_absent(headers: &[GatewayHeader], name: &str) {
    assert!(header_value(headers, name).is_none(), "unexpected {name}");
}

fn assert_contains(haystack: &str, needle: &str) {
    assert!(haystack.contains(needle), "missing {needle}");
}

fn assert_absent(haystack: &str, needle: &str) {
    assert!(!haystack.contains(needle), "unexpected {needle}");
}
