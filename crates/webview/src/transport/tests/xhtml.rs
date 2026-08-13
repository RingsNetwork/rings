use super::*;

#[test]
fn bootstrap_is_xml_cdata_and_svg_gets_no_html_runtime() -> Result<()> {
    let target = TargetUrl::parse("https://example.com/index")?.into_url();
    let xhtml = ConcurrentWebviewGateway::new(
        GatewayPrefix::new("/webview/")?,
        DocumentTransport {
            content_type: Some("application/xhtml+xml"),
            body: br#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head></head><body></body></html>"#.to_vec(),
        },
    )
    .with_bootstrap_script(
        "const marker = \"]]>\"; if (left && right) { globalThis.ready = marker; }",
    );
    let response =
        futures::executor::block_on(xhtml.send(GatewayRequest::navigation(target.clone())))?;
    let body = String::from_utf8(response.body)
        .map_err(|error| WebviewError::transport(error.to_string()))?;
    assert!(body.starts_with("<?xml version=\"1.0\"?>"));
    assert!(body.contains("data-rings-webview-bootstrap"));
    assert!(body.contains("<![CDATA["));
    assert!(body.contains("]]>") && body.contains("&&"));
    assert!(body.contains("]]]]><![CDATA[>"));

    let headless_xhtml = ConcurrentWebviewGateway::new(
        GatewayPrefix::new("/webview/")?,
        DocumentTransport {
            content_type: Some("application/xhtml+xml"),
            body: br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><img src="/image.png"/></body></html>"#.to_vec(),
        },
    )
    .with_bootstrap_script("globalThis.mustStayInsideTheRoot = true;");
    let response = futures::executor::block_on(
        headless_xhtml.send(GatewayRequest::navigation(target.clone())),
    )?;
    let body = String::from_utf8(response.body)
        .map_err(|error| WebviewError::transport(error.to_string()))?;
    assert!(body.contains("/webview/"));
    let bootstrap = body
        .find("data-rings-webview-bootstrap")
        .ok_or_else(|| WebviewError::transport("missing headless XHTML bootstrap"))?;
    let body_end = body
        .find("</body>")
        .ok_or_else(|| WebviewError::transport("missing headless XHTML body end"))?;
    assert!(
        bootstrap < body_end,
        "XHTML bootstrap must stay inside the document root"
    );
    assert!(body.trim_end().ends_with("</html>"));

    let svg = ConcurrentWebviewGateway::new(GatewayPrefix::new("/webview/")?, DocumentTransport {
        content_type: Some("image/svg+xml"),
        body: br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><image href="/image.png"/></svg>"#.to_vec(),
    })
    .with_bootstrap_script("globalThis.htmlOnly = true;");
    let response = futures::executor::block_on(svg.send(GatewayRequest::navigation(target)))?;
    let body = String::from_utf8(response.body)
        .map_err(|error| WebviewError::transport(error.to_string()))?;
    assert!(body.contains("/webview/"));
    assert!(body.contains("viewBox=\"0 0 10 10\""));
    assert!(body.contains(" />"));
    assert!(body.trim_end().ends_with("</svg>"));
    assert!(!body.contains("data-rings-webview-bootstrap"));
    Ok(())
}
