use serde::Deserialize;
use serde::Serialize;
use url::Url;

use crate::error::Result;
use crate::error::WebviewError;

/// One HTTP header passed through the gateway boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GatewayHeader {
    /// Header name.
    pub name: String,
    /// Header value.
    pub value: String,
}

impl GatewayHeader {
    /// Build a validated gateway header.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(WebviewError::Header("header name is empty".to_string()));
        }
        if name.chars().any(|ch| ch.is_control()) {
            return Err(WebviewError::Header(format!(
                "header name {name:?} contains control characters"
            )));
        }
        let value = value.into();
        if value.chars().any(|ch| ch == '\r' || ch == '\n') {
            return Err(WebviewError::Header(format!(
                "header {name:?} value contains newline"
            )));
        }
        Ok(Self { name, value })
    }

    /// Return true when this header has `name`, ignoring ASCII case.
    pub fn name_eq(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }
}

/// Browser request class that produced a gateway request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum GatewayRequestKind {
    /// Top-level document navigation.
    Navigation,
    /// Static page subresource such as image, stylesheet, or script.
    Subresource,
    /// Runtime `fetch` request.
    Fetch,
    /// Runtime `XMLHttpRequest`.
    Xhr,
}

/// Credential mode captured from a browser runtime request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum GatewayCredentials {
    /// Never attach or accept target cookies for this request.
    Omit,
    /// Attach target cookies only when the virtual source and target origins match.
    SameOrigin,
    /// Attach target cookies even for a permitted cross-origin request.
    Include,
}

/// Normalized request passed from the controlled origin to a gateway transport.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct GatewayRequest {
    /// Absolute target URL.
    pub target: Url,
    /// HTTP method.
    pub method: String,
    /// Request headers.
    pub headers: Vec<GatewayHeader>,
    /// Request body bytes.
    pub body: Vec<u8>,
    /// Browser request class.
    pub kind: GatewayRequestKind,
    /// Trusted source origin for requests initiated by another target document.
    ///
    /// This URL is normalized to the origin root (`scheme://host[:port]/`). The gateway
    /// serializes only that origin upstream and uses it to apply virtual CORS, SameSite, and
    /// credential rules. It must never be populated from an untrusted page header.
    pub source_origin: Option<Url>,
    /// Trusted page target that initiated this request.
    ///
    /// Unlike `source_origin`, this preserves the concrete controlled page URL for diagnostics
    /// such as scoped WebView network and onion-route logs. It must never be populated from an
    /// untrusted page header.
    pub source_target: Option<Url>,
    /// Browser credential mode for this request.
    pub credentials: GatewayCredentials,
    /// Whether this request is a top-level document navigation rather than an iframe navigation.
    pub top_level_navigation: bool,
}

impl GatewayRequest {
    /// Build a gateway request for `target`, `method`, and browser request `kind`.
    pub fn new(target: Url, method: impl Into<String>, kind: GatewayRequestKind) -> Self {
        Self {
            target,
            method: method.into(),
            headers: Vec::new(),
            body: Vec::new(),
            kind,
            source_origin: None,
            source_target: None,
            credentials: GatewayCredentials::SameOrigin,
            top_level_navigation: kind == GatewayRequestKind::Navigation,
        }
    }

    /// Build a GET navigation request for `target`.
    pub fn navigation(target: Url) -> Self {
        Self::new(target, "GET", GatewayRequestKind::Navigation)
    }

    /// Build a GET subresource request for `target`.
    pub fn subresource(target: Url) -> Self {
        Self::new(target, "GET", GatewayRequestKind::Subresource)
    }

    /// Build a runtime `fetch` request for `target`.
    pub fn fetch(target: Url, method: impl Into<String>) -> Self {
        Self::new(target, method, GatewayRequestKind::Fetch)
    }

    /// Build a runtime `XMLHttpRequest` request for `target`.
    pub fn xhr(target: Url, method: impl Into<String>) -> Self {
        Self::new(target, method, GatewayRequestKind::Xhr)
    }

    /// Attach a request header.
    pub fn with_header(mut self, header: GatewayHeader) -> Self {
        self.headers.push(header);
        self
    }

    /// Attach a request body.
    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// Attach the trusted virtual source origin for a browser runtime request.
    pub fn with_source_origin(mut self, source_origin: Url) -> Self {
        self.source_origin = Some(normalize_origin_url(source_origin));
        self
    }

    /// Normalize trusted source context after direct struct construction.
    pub fn normalize_source_origin(mut self) -> Self {
        if let Some(source_origin) = self.source_origin.take() {
            self.source_origin = Some(normalize_origin_url(source_origin));
        }
        self
    }

    /// Set the credential mode captured from a browser runtime request.
    pub fn with_credentials(mut self, credentials: GatewayCredentials) -> Self {
        self.credentials = credentials;
        self
    }

    /// Mark whether this request is a top-level document navigation.
    pub fn with_top_level_navigation(mut self, top_level_navigation: bool) -> Self {
        self.top_level_navigation = top_level_navigation;
        self
    }

    /// Return true when this runtime request crosses virtual target origins.
    pub fn is_cross_origin_runtime_request(&self) -> bool {
        matches!(
            self.kind,
            GatewayRequestKind::Fetch | GatewayRequestKind::Xhr
        ) && self
            .source_origin
            .as_ref()
            .is_some_and(|source| source.origin() != self.target.origin())
    }

    /// Return true when runtime CORS/SameSite context is missing.
    pub fn lacks_runtime_source_origin(&self) -> bool {
        matches!(
            self.kind,
            GatewayRequestKind::Fetch | GatewayRequestKind::Xhr
        ) && self.source_origin.is_none()
    }

    /// Return whether the gateway may attach or store target cookies for this request.
    pub fn allows_target_cookies(&self) -> bool {
        if !matches!(
            self.kind,
            GatewayRequestKind::Fetch | GatewayRequestKind::Xhr
        ) {
            return true;
        }
        match (self.source_origin.as_ref(), self.credentials) {
            (None, _) => false,
            (_, GatewayCredentials::Omit) => false,
            (Some(_), GatewayCredentials::Include) => true,
            (Some(source), GatewayCredentials::SameOrigin) => {
                source.origin() == self.target.origin()
            }
        }
    }
}

fn normalize_origin_url(mut url: Url) -> Url {
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    url
}

/// Normalized response returned by a gateway transport.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GatewayResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers after transport execution.
    pub headers: Vec<GatewayHeader>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

impl GatewayResponse {
    /// Build a gateway response.
    pub fn new(status: u16, headers: Vec<GatewayHeader>, body: Vec<u8>) -> Result<Self> {
        if !(100..=599).contains(&status) {
            return Err(WebviewError::Header(format!(
                "invalid response status {status}"
            )));
        }
        Ok(Self {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_origin_is_normalized_to_origin_root() -> Result<()> {
        let request = GatewayRequest::fetch(Url::parse("https://api.example.test/data")?, "GET")
            .with_source_origin(Url::parse(
                "https://user:pass@app.example.test:8443/path/page?q=1#section",
            )?);

        assert_eq!(
            request.source_origin.as_ref().map(Url::as_str),
            Some("https://app.example.test:8443/")
        );
        Ok(())
    }

    #[test]
    fn test_source_less_runtime_requests_do_not_allow_target_cookies() -> Result<()> {
        let request = GatewayRequest::fetch(Url::parse("https://api.example.test/data")?, "GET")
            .with_credentials(GatewayCredentials::Include);

        assert!(request.lacks_runtime_source_origin());
        assert!(!request.allows_target_cookies());
        Ok(())
    }
}
