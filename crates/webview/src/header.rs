use url::Url;

use crate::error::Result;
use crate::rewrite::rewrite_refresh_value;
use crate::types::GatewayHeader;
use crate::types::GatewayRequest;
use crate::types::GatewayResponse;
use crate::url::GatewayPrefix;

const GATEWAY_CONTENT_SECURITY_POLICY: &str = "default-src 'self' data: blob:; base-uri 'self'; connect-src 'self'; font-src 'self' data:; form-action 'self'; frame-src 'self' data: blob:; img-src 'self' data: blob:; media-src 'self' data: blob:; object-src 'self'; script-src 'self' data: 'unsafe-inline' 'unsafe-eval' 'wasm-unsafe-eval' blob:; style-src 'self' data: 'unsafe-inline'; worker-src 'none'";

/// Header policy for controlled webview documents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaderPolicy {
    gateway_prefix: GatewayPrefix,
}

impl HeaderPolicy {
    /// Build a header policy that rewrites target redirects into `gateway_prefix`.
    pub fn new(gateway_prefix: GatewayPrefix) -> Self {
        Self { gateway_prefix }
    }

    /// Normalize controlled-origin request headers before they reach a target transport.
    pub fn normalize_request(&self, mut request: GatewayRequest) -> GatewayRequest {
        request
            .headers
            .retain(|header| !should_strip_request_header(header.name.as_str()));
        request.headers.push(GatewayHeader {
            name: "Accept-Encoding".to_string(),
            value: "identity".to_string(),
        });
        if should_send_virtual_origin(&request) {
            let Some(source_origin) = request.source_origin.as_ref() else {
                return request;
            };
            request.headers.push(GatewayHeader {
                name: "Origin".to_string(),
                value: source_origin.origin().ascii_serialization(),
            });
        }
        request
    }

    /// Normalize transport response headers for a proxied target URL.
    pub fn normalize_response(
        &self,
        target: &Url,
        response: GatewayResponse,
    ) -> Result<GatewayResponse> {
        let mut headers = Vec::new();
        for header in response.headers {
            if should_strip_response_header(header.name.as_str()) {
                continue;
            }
            if header.name_eq("location") {
                if let Some(location) = self
                    .gateway_prefix
                    .rewrite_url_value(target, header.value.as_str())?
                {
                    headers.push(GatewayHeader::new(header.name, location)?);
                }
                continue;
            }
            if header.name_eq("refresh") {
                headers.push(GatewayHeader::new(
                    header.name,
                    rewrite_refresh_value(header.value.as_str(), target, &self.gateway_prefix)?,
                )?);
                continue;
            }
            headers.push(header);
        }
        headers.push(GatewayHeader::new(
            "Content-Security-Policy",
            GATEWAY_CONTENT_SECURITY_POLICY,
        )?);
        GatewayResponse::new(response.status, headers, response.body)
    }
}

fn should_strip_request_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("sec-fetch-")
        || matches!(
            lower.as_str(),
            "accept-encoding"
                | "connection"
                | "content-length"
                | "cookie"
                | "expect"
                | "host"
                | "keep-alive"
                | "origin"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "proxy-connection"
                | "referer"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
                | "via"
                | "x-rings-webview-kind"
                | "x-rings-webview-target"
        )
}

fn should_send_virtual_origin(request: &GatewayRequest) -> bool {
    request.is_cross_origin_runtime_request()
}

fn should_strip_response_header(name: &str) -> bool {
    const STRIPPED: &[&str] = &[
        "connection",
        "content-encoding",
        "content-length",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-security-policy",
        "content-security-policy-report-only",
        "x-frame-options",
        "strict-transport-security",
        "set-cookie",
    ];
    STRIPPED
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::GatewayRequestKind;

    #[test]
    fn redirect_location_rewrites_to_gateway_url() -> Result<()> {
        let target = Url::parse("https://example.com/app/page")?;
        let policy = HeaderPolicy::new(GatewayPrefix::new("/webview/")?);
        let response = GatewayResponse::new(
            302,
            vec![
                GatewayHeader::new("Location", "../login?next=1")?,
                GatewayHeader::new("Content-Security-Policy", "default-src 'none'")?,
                GatewayHeader::new("Content-Type", "text/html")?,
            ],
            Vec::new(),
        )?;

        let normalized = policy.normalize_response(&target, response)?;

        assert_eq!(normalized.headers.len(), 3);
        assert!(normalized
            .headers
            .iter()
            .any(|header| { header.name_eq("location") && header.value.starts_with("/webview/") }));
        assert!(normalized.headers.iter().any(|header| {
            header.name_eq("content-security-policy")
                && header.value == GATEWAY_CONTENT_SECURITY_POLICY
        }));
        Ok(())
    }

    #[test]
    fn response_policy_strips_headers_invalidated_by_gateway_rewrites() -> Result<()> {
        let target = Url::parse("https://example.com/app/page")?;
        let policy = HeaderPolicy::new(GatewayPrefix::new("/webview/")?);
        let response = GatewayResponse::new(
            200,
            vec![
                GatewayHeader::new("Content-Encoding", "gzip")?,
                GatewayHeader::new("Content-Length", "128")?,
                GatewayHeader::new("Content-Type", "text/html")?,
                GatewayHeader::new("X-Frame-Options", "DENY")?,
            ],
            Vec::new(),
        )?;

        let normalized = policy.normalize_response(&target, response)?;

        assert!(normalized.headers.iter().all(|header| {
            !matches!(
                header.name.to_ascii_lowercase().as_str(),
                "content-encoding" | "content-length" | "x-frame-options"
            )
        }));
        assert!(normalized
            .headers
            .iter()
            .any(|header| header.name_eq("content-type") && header.value == "text/html"));
        assert!(normalized.headers.iter().any(|header| {
            header.name_eq("content-security-policy")
                && header.value == GATEWAY_CONTENT_SECURITY_POLICY
        }));
        Ok(())
    }

    #[test]
    fn refresh_header_rewrites_to_gateway_url() -> Result<()> {
        let target = Url::parse("https://example.com/app/page")?;
        let policy = HeaderPolicy::new(GatewayPrefix::new("/webview/")?);
        let response = GatewayResponse::new(
            200,
            vec![GatewayHeader::new("Refresh", "0; URL='../login?next=1'")?],
            Vec::new(),
        )?;

        let normalized = policy.normalize_response(&target, response)?;

        let refresh = normalized
            .headers
            .iter()
            .find(|header| header.name_eq("refresh"))
            .ok_or_else(|| {
                crate::error::WebviewError::Header("missing refresh header".to_string())
            })?;
        assert!(refresh
            .value
            .contains("/webview/https%3A%2F%2Fexample%2Ecom%2Flogin%3Fnext%3D1"));
        assert!(!refresh.value.contains("../login?next=1"));
        Ok(())
    }

    #[test]
    fn request_policy_strips_controlled_origin_and_hop_headers() -> Result<()> {
        let target = Url::parse("https://example.com/app/page")?;
        let policy = HeaderPolicy::new(GatewayPrefix::new("/webview/")?);
        let request = GatewayRequest {
            target,
            method: "GET".to_string(),
            headers: vec![
                GatewayHeader::new("Host", "127.0.0.1:3000")?,
                GatewayHeader::new("Origin", "http://127.0.0.1:3000")?,
                GatewayHeader::new("Referer", "http://127.0.0.1:3000/webview/x")?,
                GatewayHeader::new("Sec-Fetch-Dest", "document")?,
                GatewayHeader::new("Cookie", "caller=leak")?,
                GatewayHeader::new("Accept-Encoding", "gzip")?,
                GatewayHeader::new("X-Rings-Webview-Kind", "fetch")?,
                GatewayHeader::new("X-Rings-Webview-Target", "https://example.com/app/page")?,
                GatewayHeader::new("Accept", "text/html")?,
                GatewayHeader::new("X-App-Trace", "kept")?,
            ],
            body: Vec::new(),
            kind: GatewayRequestKind::Navigation,
            source_origin: None,
            source_target: None,
            credentials: crate::types::GatewayCredentials::SameOrigin,
            top_level_navigation: true,
        };

        let normalized = policy.normalize_request(request);

        assert!(normalized.headers.iter().all(|header| !matches!(
            header.name.to_ascii_lowercase().as_str(),
            "host"
                | "origin"
                | "referer"
                | "sec-fetch-dest"
                | "cookie"
                | "x-rings-webview-kind"
                | "x-rings-webview-target"
        )));
        assert!(normalized
            .headers
            .iter()
            .any(|header| header.name_eq("accept") && header.value == "text/html"));
        assert!(normalized.headers.iter().any(|header| {
            header.name_eq("accept-encoding") && header.value.eq_ignore_ascii_case("identity")
        }));
        assert!(normalized
            .headers
            .iter()
            .any(|header| header.name_eq("x-app-trace") && header.value == "kept"));
        Ok(())
    }

    #[test]
    fn request_policy_does_not_synthesize_origin_for_subresources() -> Result<()> {
        let target = Url::parse("https://cdn.example.test/app.js")?;
        let policy = HeaderPolicy::new(GatewayPrefix::new("/webview/")?);
        let request = GatewayRequest::subresource(target)
            .with_source_origin(Url::parse("https://app.example.test/page")?);

        let normalized = policy.normalize_request(request);

        assert!(normalized
            .headers
            .iter()
            .all(|header| !header.name_eq("origin")));
        Ok(())
    }
}
