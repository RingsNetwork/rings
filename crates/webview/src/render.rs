//! Render target pages into gateway-rewritten HTML.

use url::Url;

use crate::error::Result;
use crate::error::WebviewError;
use crate::transport::GatewayTransport;
use crate::transport::WebviewGateway;
use crate::types::GatewayHeader;
use crate::types::GatewayRequest;
use crate::url::TargetUrl;

/// A target page rendered through the webview gateway.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedPage {
    /// Original target URL.
    pub target: Url,
    /// Controlled-origin URL that represents the target.
    pub gateway_url: String,
    /// Upstream status after gateway policy normalization.
    pub status: u16,
    /// Response headers after gateway policy normalization.
    pub headers: Vec<GatewayHeader>,
    html: String,
}

impl RenderedPage {
    /// Borrow the rewritten HTML body suitable for a controlled renderer.
    pub fn html(&self) -> &str {
        &self.html
    }

    /// Consume this page and return the rewritten HTML body.
    pub fn into_html(self) -> String {
        self.html
    }
}

/// Page renderer backed by a reusable gateway transport.
pub struct WebviewRenderer<T> {
    gateway: WebviewGateway<T>,
}

impl<T> WebviewRenderer<T>
where T: GatewayTransport
{
    /// Build a renderer around an existing gateway.
    pub fn new(gateway: WebviewGateway<T>) -> Self {
        Self { gateway }
    }

    /// Access the underlying gateway policy state.
    pub fn gateway(&self) -> &WebviewGateway<T> {
        &self.gateway
    }

    /// Mutably access the underlying gateway policy state.
    pub fn gateway_mut(&mut self) -> &mut WebviewGateway<T> {
        &mut self.gateway
    }

    /// Render a target page into gateway-rewritten HTML.
    pub async fn render(&mut self, target: TargetUrl) -> Result<RenderedPage> {
        let target = target.into_url();
        let gateway_url = self.gateway.prefix().encode(&target);
        let response = self
            .gateway
            .send(GatewayRequest::navigation(target.clone()))
            .await?;
        let html = String::from_utf8(response.body).map_err(|error| {
            WebviewError::Render(format!("HTML response is not UTF-8: {error}"))
        })?;
        Ok(RenderedPage {
            target,
            gateway_url,
            status: response.status,
            headers: response.headers,
            html,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use async_trait::async_trait;

    use super::*;
    use crate::types::GatewayResponse;
    use crate::GatewayPrefix;

    struct FixtureTransport {
        requests: RefCell<Vec<GatewayRequest>>,
    }

    impl FixtureTransport {
        fn new() -> Self {
            Self {
                requests: RefCell::new(Vec::new()),
            }
        }
    }

    #[async_trait(?Send)]
    impl GatewayTransport for FixtureTransport {
        async fn send(
            &self,
            request: GatewayRequest,
            _body_limit: crate::transport::GatewayResponseBodyLimit,
        ) -> Result<GatewayResponse> {
            self.requests.borrow_mut().push(request);
            GatewayResponse::new(
                200,
                vec![
                    GatewayHeader::new("Content-Type", "text/html; charset=utf-8")?,
                    GatewayHeader::new("Set-Cookie", "sid=fixture; Path=/docs; Secure")?,
                    GatewayHeader::new("Content-Security-Policy", "default-src 'none'")?,
                ],
                br#"<!doctype html>
<html>
  <head>
    <link rel="stylesheet" href="/assets/site.css">
    <script src="app.js"></script>
  </head>
  <body>
    <a href="../next">next</a>
    <img srcset="thumb.png 1x, /hero.png 2x">
  </body>
</html>"#
                    .to_vec(),
            )
        }
    }

    #[test]
    fn renderer_outputs_gateway_rewritten_html_page() -> Result<()> {
        let gateway =
            WebviewGateway::new(GatewayPrefix::new("/webview/")?, FixtureTransport::new())
                .with_bootstrap_script("globalThis.__ringsWebview = true;");
        let mut renderer = WebviewRenderer::new(gateway);

        let page = futures::executor::block_on(
            renderer.render(TargetUrl::parse("https://example.test/docs/index.html")?),
        )?;

        assert_eq!(page.status, 200);
        assert!(page.gateway_url.starts_with("/webview/"));
        assert!(page.html().contains("data-rings-webview-bootstrap"));
        assert!(page
            .html()
            .contains("/webview/https%3A%2F%2Fexample%2Etest%2Fassets%2Fsite%2Ecss"));
        assert!(page
            .html()
            .contains("/webview/https%3A%2F%2Fexample%2Etest%2Fdocs%2Fapp%2Ejs"));
        assert!(page
            .html()
            .contains("/webview/https%3A%2F%2Fexample%2Etest%2Fnext"));
        assert!(page
            .html()
            .contains("/webview/https%3A%2F%2Fexample%2Etest%2Fhero%2Epng"));
        assert!(!page
            .headers
            .iter()
            .any(|header| header.name_eq("set-cookie")));
        assert!(page.headers.iter().any(|header| {
            header.name_eq("content-security-policy") && header.value.contains("connect-src 'self'")
        }));
        assert_eq!(renderer.gateway().cookies().len(), 1);
        Ok(())
    }
}
