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

/// Bound on every HTTP request to a remote endpoint, and on the resolution of its host, so an
/// endpoint that accepts a connection and never answers, or a resolver that never does, cannot
/// hold a caller indefinitely.
#[cfg(rings_native)]
const REMOTE_RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A JSON-RPC client for `endpoint`: no proxy, no redirects, a request timeout, the host pinned
/// to the addresses the public-target resolver returned, and the bearer token attached when
/// given.
#[cfg(rings_native)]
pub(crate) async fn remote_rpc_client(
    endpoint: &RemoteRpcEndpoint,
    api_token: Option<&str>,
) -> Result<rings_rpc::jsonrpc::Client> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REMOTE_RPC_TIMEOUT);
    if let Some(target) = resolution_target(&endpoint.0)? {
        let addresses = tokio::time::timeout(
            REMOTE_RPC_TIMEOUT,
            crate::onion::target::resolve_public_target(&target.lookup),
        )
        .await
        .map_err(|_| Error::RemoteRpcError("endpoint resolution timed out".to_string()))??;
        builder = builder.resolve_to_addrs(&target.pin_host, &addresses);
    }
    let http_client = builder
        .build()
        .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
    let client = rings_rpc::jsonrpc::Client::with_http_client(endpoint.as_str(), http_client);
    Ok(match api_token {
        Some(token) => client.with_bearer_token(token.to_owned()),
        None => client,
    })
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
/// enforces its own origin policy.
#[cfg(rings_browser)]
pub(crate) async fn remote_rpc_client(
    endpoint: &RemoteRpcEndpoint,
    api_token: Option<&str>,
) -> Result<rings_rpc::jsonrpc::Client> {
    let client = rings_rpc::jsonrpc::Client::new(endpoint.as_str());
    Ok(match api_token {
        Some(token) => client.with_bearer_token(token.to_owned()),
        None => client,
    })
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
}
