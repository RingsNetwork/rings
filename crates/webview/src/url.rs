use percent_encoding::percent_decode_str;
use percent_encoding::utf8_percent_encode;
use percent_encoding::NON_ALPHANUMERIC;
use serde::Deserialize;
use serde::Serialize;
use url::Url;

use crate::error::Result;
use crate::error::WebviewError;

/// Absolute target URL accepted by the gateway.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TargetUrl {
    inner: Url,
}

impl TargetUrl {
    /// Parse and validate an absolute HTTP(S) target URL.
    pub fn parse(input: &str) -> Result<Self> {
        let inner = Url::parse(input.trim())?;
        match inner.scheme() {
            "http" | "https" => Ok(Self { inner }),
            scheme => Err(WebviewError::UnsupportedScheme(scheme.to_string())),
        }
    }

    /// Borrow the underlying URL.
    pub fn as_url(&self) -> &Url {
        &self.inner
    }

    /// Consume this wrapper and return the URL.
    pub fn into_url(self) -> Url {
        self.inner
    }
}

/// Rings-owned route prefix used to serve controlled webview pages.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GatewayPrefix {
    prefix: String,
}

impl GatewayPrefix {
    /// Build a gateway prefix such as `/webview/`.
    pub fn new(prefix: impl Into<String>) -> Result<Self> {
        let mut prefix = prefix.into();
        if !prefix.starts_with('/') || prefix.contains('?') || prefix.contains('#') {
            return Err(WebviewError::InvalidGatewayPrefix(prefix));
        }
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        Ok(Self { prefix })
    }

    /// Return the route prefix.
    pub fn as_str(&self) -> &str {
        &self.prefix
    }

    /// Encode `target` into a local controlled-origin gateway URL.
    pub fn encode(&self, target: &Url) -> String {
        format!(
            "{}{}",
            self.prefix,
            utf8_percent_encode(target.as_str(), NON_ALPHANUMERIC)
        )
    }

    /// Decode the target URL embedded in a local gateway path.
    pub fn decode_path(&self, path: &str) -> Result<TargetUrl> {
        let Some(encoded) = path.strip_prefix(self.prefix.as_str()) else {
            return Err(WebviewError::InvalidGatewayUrl(path.to_string()));
        };
        if encoded.is_empty() {
            return Err(WebviewError::InvalidGatewayUrl(path.to_string()));
        }
        let decoded = percent_decode_str(encoded)
            .decode_utf8()
            .map_err(|error| WebviewError::Decode(error.to_string()))?;
        TargetUrl::parse(decoded.as_ref())
    }

    /// Resolve a page URL-bearing value against `document_url`.
    pub fn resolve_url_value(&self, document_url: &Url, value: &str) -> Result<Option<Url>> {
        let trimmed = value.trim();
        if trimmed.is_empty() || is_unrewritable_url(trimmed) {
            return Ok(None);
        }
        let target = document_url.join(trimmed)?;
        match target.scheme() {
            "http" | "https" => Ok(Some(target)),
            _ => Ok(None),
        }
    }

    /// Resolve a page URL-bearing value against `document_url` and encode it for the gateway.
    pub fn rewrite_url_value(&self, document_url: &Url, value: &str) -> Result<Option<String>> {
        Ok(self
            .resolve_url_value(document_url, value)?
            .map(|target| self.encode(&target)))
    }
}

fn is_unrewritable_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("javascript:")
        || lower.starts_with("mailto:")
        || lower.starts_with("tel:")
        || lower.starts_with("data:")
        || lower.starts_with("blob:")
        || lower.starts_with("about:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_url_round_trips_absolute_target() -> Result<()> {
        let prefix = GatewayPrefix::new("/webview")?;
        let target = TargetUrl::parse("https://example.com/a path/?q=1#section")?;

        let gateway_url = prefix.encode(target.as_url());
        let decoded = prefix.decode_path(&gateway_url)?;

        assert_eq!(decoded, target);
        assert!(gateway_url.starts_with("/webview/"));
        Ok(())
    }

    #[test]
    fn gateway_rejects_non_http_targets() {
        assert!(matches!(
            TargetUrl::parse("file:///tmp/index.html"),
            Err(WebviewError::UnsupportedScheme(_))
        ));
    }

    #[test]
    fn relative_url_rewrites_against_document_url() -> Result<()> {
        let prefix = GatewayPrefix::new("/webview/")?;
        let document = TargetUrl::parse("https://example.com/a/page.html")?;

        let rewritten = prefix
            .rewrite_url_value(document.as_url(), "next.html?x=1")?
            .ok_or_else(|| WebviewError::InvalidGatewayUrl("missing rewrite".to_string()))?;

        assert_eq!(
            prefix.decode_path(&rewritten)?.as_url().as_str(),
            "https://example.com/a/next.html?x=1"
        );
        Ok(())
    }
}
