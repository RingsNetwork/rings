//! Public HTTP(S) JSON-RPC endpoints of remote nodes: the validated form and the client over it.
//!
//! [`RemoteRpcEndpoint`] is the proof that a URL passed the endpoint policy — HTTP(S) scheme,
//! no credentials or fragment, a public host — and every client is built from that proof, so
//! the policy runs once, where a string enters, and the proof travels as a type.

use std::fmt;

use crate::error::Error;
use crate::error::Result;

/// A URL that passed the remote RPC endpoint policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRpcEndpoint(reqwest::Url);

impl RemoteRpcEndpoint {
    /// Parse `url` as a public HTTP(S) RPC endpoint: no credentials, no fragment, and a host
    /// the network policy permits.
    pub fn parse(url: &str) -> Result<Self> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|error| Error::UnsafeRemoteRpcTarget(error.to_string()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(Error::UnsafeRemoteRpcTarget(
                "only HTTP(S) endpoints are supported".to_string(),
            ));
        }
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
        {
            return Err(Error::UnsafeRemoteRpcTarget(
                "credentials and fragments are not permitted in an RPC endpoint URL".to_string(),
            ));
        }
        rings_network_policy::validate_public_url_host(&parsed)
            .map_err(|error| Error::UnsafeRemoteRpcTarget(error.to_string()))?;
        Ok(Self(parsed))
    }

    /// The endpoint as a URL string.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for RemoteRpcEndpoint {
    /// Render the endpoint as its URL.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An HTTP(S) proxy URL that passed the remote RPC proxy policy: `http` or `https` scheme, a host
/// and usable port, root path (`/`), and no credentials, query parameters, or fragment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRpcProxy(reqwest::Url);

impl RemoteRpcProxy {
    /// Parse `url` as an HTTP(S) proxy for remote RPC endpoints.
    pub fn parse(url: &str) -> Result<Self> {
        let parsed = reqwest::Url::parse(url).map_err(|error| {
            Error::InvalidConfig(format!("invalid remote_rpc_proxy_url: {error}"))
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(Error::InvalidConfig(
                "remote_rpc_proxy_url must use http:// or https://".to_string(),
            ));
        }
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(Error::InvalidConfig(
                "credentials, non-root paths, query parameters, and fragments are not permitted \
                 in remote_rpc_proxy_url"
                    .to_string(),
            ));
        }
        if parsed.host().is_none() || parsed.port_or_known_default().is_none() {
            return Err(Error::InvalidConfig(
                "remote_rpc_proxy_url must include a host and usable port".to_string(),
            ));
        }
        #[cfg(rings_native)]
        reqwest::Proxy::all(parsed.as_str()).map_err(|error| {
            Error::InvalidConfig(format!("invalid remote_rpc_proxy_url: {error}"))
        })?;
        Ok(Self(parsed))
    }

    /// The proxy URL as a string.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Build the `reqwest::Proxy` for this proxy URL.
    #[cfg(rings_native)]
    pub(crate) fn to_reqwest_proxy(&self) -> Result<reqwest::Proxy> {
        reqwest::Proxy::all(self.as_str()).map_err(|error| Error::RemoteRpcError(error.to_string()))
    }
}

impl fmt::Display for RemoteRpcProxy {
    /// Render the proxy as its URL.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Bound on every HTTP request to a remote endpoint, and on the resolution of its host, so an
/// endpoint that accepts a connection and never answers, or a resolver that never does, cannot
/// hold a caller indefinitely.
#[cfg(rings_native)]
const REMOTE_RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A JSON-RPC client for `endpoint`: no redirects, a request timeout, and the bearer token
/// attached when given.
///
/// By default (`proxy = None`), ambient proxy variables are ignored and a domain endpoint is
/// resolved once through the public-target resolver and pinned to the returned addresses. When
/// `proxy` is `Some`, requests are routed through that explicit HTTP(S) proxy and local DNS
/// pinning is skipped so the proxy resolves the target (for example on `CONNECT host:port`),
/// while [`RemoteRpcEndpoint`] still enforces the public-host URL policy before any request is
/// sent.
#[cfg(rings_native)]
pub(crate) async fn remote_rpc_client(
    endpoint: &RemoteRpcEndpoint,
    api_token: Option<&str>,
    proxy: Option<&RemoteRpcProxy>,
) -> Result<rings_rpc::jsonrpc::Client> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REMOTE_RPC_TIMEOUT);
    let target = resolution_target(&endpoint.0)?;
    let client = match proxy {
        Some(proxy) => rings_rpc::jsonrpc::Client::with_http_client_builder_and_proxy(
            endpoint.as_str(),
            builder,
            proxy.to_reqwest_proxy()?,
        ),
        None => {
            if let Some(target) = target {
                let addresses = tokio::time::timeout(
                    REMOTE_RPC_TIMEOUT,
                    crate::onion::target::resolve_public_target(&target.lookup),
                )
                .await
                .map_err(|_| {
                    Error::RemoteRpcError("endpoint resolution timed out".to_string())
                })??;
                builder = builder.resolve_to_addrs(&target.pin_host, &addresses);
            }
            rings_rpc::jsonrpc::Client::with_http_client_builder(endpoint.as_str(), builder)
        }
    }
    .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
    match api_token {
        Some(token) => client
            .with_bearer_token(token.to_owned())
            .map_err(|error| Error::RemoteRpcError(error.to_string())),
        None => Ok(client),
    }
}

/// The DNS resolution to pin for a domain endpoint; `None` for an IP literal, which needs no
/// resolution and therefore no pin.
#[cfg(rings_native)]
fn resolution_target(parsed: &reqwest::Url) -> Result<Option<ResolutionTarget>> {
    let port = parsed.port_or_known_default().ok_or_else(|| {
        Error::UnsafeRemoteRpcTarget("endpoint URL has no usable port".to_string())
    })?;
    let no_host = || Error::UnsafeRemoteRpcTarget("endpoint URL has no host".to_string());
    let pin_host = parsed.host_str().ok_or_else(no_host)?;
    match parsed.host().ok_or_else(no_host)? {
        url::Host::Domain(host) => crate::onion::OnionProxyTarget::new(host, port).map(|lookup| {
            Some(ResolutionTarget {
                lookup,
                pin_host: pin_host.to_string(),
            })
        }),
        url::Host::Ipv4(_) | url::Host::Ipv6(_) => Ok(None),
    }
}

/// A domain endpoint's resolver target and the host name its addresses are pinned under.
#[cfg(rings_native)]
#[derive(Debug, Eq, PartialEq)]
struct ResolutionTarget {
    /// The canonical host and port the public-target resolver looks up.
    lookup: crate::onion::OnionProxyTarget,
    /// The host name exactly as the HTTP client connects with it, the key its pin lives under.
    pin_host: String,
}

/// A JSON-RPC client for `endpoint` with the bearer token attached when given; the browser
/// enforces its own origin and proxy policy and rejects an explicit `remote_rpc_proxy_url`.
#[cfg(rings_browser)]
pub(crate) async fn remote_rpc_client(
    endpoint: &RemoteRpcEndpoint,
    api_token: Option<&str>,
    proxy: Option<&RemoteRpcProxy>,
) -> Result<rings_rpc::jsonrpc::Client> {
    if proxy.is_some() {
        return Err(Error::InvalidConfig(
            "remote_rpc_proxy_url is supported only on native targets".to_string(),
        ));
    }
    let client = rings_rpc::jsonrpc::Client::new(endpoint.as_str())
        .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
    match api_token {
        Some(token) => client
            .with_bearer_token(token.to_owned())
            .map_err(|error| Error::RemoteRpcError(error.to_string())),
        None => Ok(client),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Local, link-local, single-label, credentialed and non-HTTP targets are refused.
    #[test]
    fn endpoint_rejects_local_targets_and_url_credentials() {
        for target in [
            "http://127.0.0.1:50001/",
            "http://169.254.169.254/latest/meta-data/",
            "https://intranet:50001/",
            "https://token@example.com:50001/",
            "file:///tmp/socket",
        ] {
            assert!(matches!(
                RemoteRpcEndpoint::parse(target),
                Err(Error::UnsafeRemoteRpcTarget(_))
            ));
        }
    }

    /// Public HTTP and HTTPS targets, by name or IP literal, are accepted.
    #[test]
    fn endpoint_accepts_public_http_and_https_targets() {
        for target in [
            "http://example.com:50001/",
            "https://1.1.1.1/rpc",
            "https://[2606:4700:4700::1111]/rpc",
        ] {
            assert!(RemoteRpcEndpoint::parse(target).is_ok());
        }
    }

    /// IP literals need no DNS pin.
    #[cfg(rings_native)]
    #[test]
    fn literal_endpoints_skip_dns_pinning() -> Result<()> {
        for target in ["https://1.1.1.1/rpc", "https://[2606:4700:4700::1111]/rpc"] {
            let endpoint = RemoteRpcEndpoint::parse(target)?;
            assert_eq!(resolution_target(&endpoint.0)?, None);
        }
        Ok(())
    }

    /// A domain endpoint resolves its host and port and pins under the connect-time host name.
    #[cfg(rings_native)]
    #[test]
    fn domain_endpoints_preserve_connect_time_host_for_dns_pin() -> Result<()> {
        let endpoint = RemoteRpcEndpoint::parse("https://example.com:8443/rpc")?;
        let target = resolution_target(&endpoint.0)?;
        assert!(matches!(
            target,
            Some(target)
                if target.lookup.host() == "example.com"
                    && target.lookup.port() == 8443
                    && target.pin_host == "example.com"
        ));
        Ok(())
    }

    /// A trailing-dot domain keeps the dotted spelling as its pin key, the one the HTTP client
    /// connects with, while resolving the canonical name.
    #[cfg(rings_native)]
    #[test]
    fn trailing_dot_domains_keep_dotted_dns_pin_key() -> Result<()> {
        let endpoint = RemoteRpcEndpoint::parse("https://example.com.:8443/rpc")?;
        let target = resolution_target(&endpoint.0)?;
        assert!(matches!(
            target,
            Some(target)
                if target.lookup.host() == "example.com"
                    && target.lookup.port() == 8443
                    && target.pin_host == "example.com."
        ));
        Ok(())
    }

    /// Non-HTTP(S), credentialed, non-root-path, query-bearing, and fragment-bearing proxy URLs
    /// are rejected.
    #[test]
    fn proxy_rejects_non_http_and_credentialed_urls() {
        for proxy_url in [
            "socks5://127.0.0.1:1080",
            "http://user:pass@127.0.0.1:8080",
            "http://127.0.0.1:8080/subpath",
            "http://127.0.0.1:8080?mode=direct",
            "http://127.0.0.1:8080#section",
            "not-a-url",
        ] {
            assert!(matches!(
                RemoteRpcProxy::parse(proxy_url),
                Err(Error::InvalidConfig(_))
            ));
        }
    }

    /// Valid HTTP and HTTPS proxy URLs are accepted.
    #[test]
    fn proxy_accepts_http_and_https_urls() {
        for proxy_url in ["http://127.0.0.1:18080", "https://proxy.example.com:8443"] {
            assert!(RemoteRpcProxy::parse(proxy_url).is_ok());
        }
    }

    /// Without an opt-in proxy, `remote_rpc_client` resolves domain endpoints locally (failing on
    /// `.test`); with `Some(proxy)`, it skips local DNS pinning and forwards the request through
    /// the configured HTTP proxy, while local `Client::new` connects directly on loopback.
    #[cfg(rings_native)]
    #[tokio::test]
    async fn remote_rpc_client_uses_proxy_only_when_opted_in() -> Result<()> {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        use crate::test_support::TestTask;

        let proxy_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
        let proxy_addr = proxy_listener
            .local_addr()
            .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
        let proxy_url = format!("http://{proxy_addr}");
        let proxy = RemoteRpcProxy::parse(proxy_url.as_str())?;

        let local_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
        let local_addr = local_listener
            .local_addr()
            .map_err(|error| Error::RemoteRpcError(error.to_string()))?;

        let unresolvable = RemoteRpcEndpoint::parse("http://seed.example.test:50001/rpc")?;
        assert!(remote_rpc_client(&unresolvable, None, None).await.is_err());

        let local_task = TestTask::spawn(async move {
            let (mut stream, _) = local_listener
                .accept()
                .await
                .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
            let mut buf = vec![0_u8; 2048];
            let read = stream
                .read(&mut buf)
                .await
                .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
            let req = String::from_utf8_lossy(buf.get(..read).unwrap_or_default()).to_string();
            let body = r#"{"jsonrpc":"2.0","result":{"did":"0x01"},"id":0}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
            Ok::<String, Error>(req)
        });

        let local_client =
            rings_rpc::jsonrpc::Client::new(format!("http://{local_addr}/").as_str())
                .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
        let request = rings_rpc::protos::rings_node::NodeDidRequest {};
        let local_resp: rings_rpc::protos::rings_node::NodeDidResponse = local_client
            .call_method(rings_rpc::method::Method::NodeDid, &request)
            .await
            .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
        assert_eq!(local_resp.did, "0x01");
        let local_request_line = local_task
            .await
            .map_err(|error| Error::RemoteRpcError(error.to_string()))??;
        assert!(local_request_line.starts_with("POST / HTTP/1.1\r\n"));

        let proxy_task = TestTask::spawn(async move {
            let (mut stream, _) = proxy_listener
                .accept()
                .await
                .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
            let mut buf = vec![0_u8; 2048];
            let read = stream
                .read(&mut buf)
                .await
                .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
            let req = String::from_utf8_lossy(buf.get(..read).unwrap_or_default()).to_string();
            let body = r#"{"jsonrpc":"2.0","result":{"did":"0x02"},"id":0}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
            Ok::<String, Error>(req)
        });

        let proxied_client = remote_rpc_client(&unresolvable, None, Some(&proxy)).await?;
        let proxied_resp: rings_rpc::protos::rings_node::NodeDidResponse = proxied_client
            .call_method(rings_rpc::method::Method::NodeDid, &request)
            .await
            .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
        assert_eq!(proxied_resp.did, "0x02");
        let proxy_request_line = proxy_task
            .await
            .map_err(|error| Error::RemoteRpcError(error.to_string()))??;
        assert!(
            proxy_request_line.starts_with("POST http://seed.example.test:50001/rpc HTTP/1.1\r\n"),
            "unexpected proxy request: {proxy_request_line}"
        );
        Ok(())
    }
}
