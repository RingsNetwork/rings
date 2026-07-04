#![warn(missing_docs)]
//! Client-side onion proxy request planning.
//!
//! This module is runtime-neutral: native can bind it to a local HTTP CONNECT listener, while
//! browser callers can use the same target and service mapping before handing bytes to a
//! browser-specific HTTPS data plane.

use rings_core::dht::Did;

use crate::error::Error;
use crate::error::Result;
use crate::onion::OnionRoute;

/// Exit service used by native HTTP CONNECT/SOCKS-style byte tunnels.
pub const ONION_PROXY_TCP_SERVICE: &str = "tcp";

/// Exit service used by browser/application-layer HTTPS fetch proxying.
pub const ONION_PROXY_HTTPS_SERVICE: &str = "https";

/// Proxy protocol requested by the client ingress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionProxyProtocol {
    /// HTTP CONNECT, SOCKS CONNECT, or any other byte tunnel. Requires a native TCP exit.
    TcpConnect,
    /// Application-layer HTTPS request proxying. This is the browser-compatible mode.
    HttpsFetch,
}

impl OnionProxyProtocol {
    /// Return the onion-exit service name required by this proxy protocol.
    pub const fn exit_service(self) -> &'static str {
        match self {
            Self::TcpConnect => ONION_PROXY_TCP_SERVICE,
            Self::HttpsFetch => ONION_PROXY_HTTPS_SERVICE,
        }
    }
}

/// Host/port target requested through an onion proxy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionProxyTarget {
    host: String,
    port: u16,
}

impl OnionProxyTarget {
    /// Parse an HTTP CONNECT authority (`host:port` or `[ipv6]:port`).
    pub fn parse_authority(authority: &str) -> Result<Self> {
        let authority = authority.trim();
        if authority.is_empty() {
            return Err(Error::HttpRequestError(
                "onion proxy target authority must not be empty".to_string(),
            ));
        }

        let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
            let Some((host, rest)) = rest.split_once(']') else {
                return Err(Error::HttpRequestError(format!(
                    "invalid IPv6 onion proxy authority {authority:?}"
                )));
            };
            let Some(port) = rest.strip_prefix(':') else {
                return Err(Error::HttpRequestError(format!(
                    "onion proxy authority {authority:?} must include a port"
                )));
            };
            (host, port)
        } else {
            authority.rsplit_once(':').ok_or_else(|| {
                Error::HttpRequestError(format!(
                    "onion proxy authority {authority:?} must be host:port"
                ))
            })?
        };

        let host = normalize_host(host)?;
        let port = port.parse::<u16>().map_err(|_| {
            Error::HttpRequestError(format!(
                "onion proxy authority {authority:?} has an invalid port"
            ))
        })?;
        if port == 0 {
            return Err(Error::HttpRequestError(
                "onion proxy target port must be non-zero".to_string(),
            ));
        }

        Ok(Self { host, port })
    }

    /// Return the normalized host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Return the target port.
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Return the canonical authority string used for exit policy/service lookup.
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn normalize_host(host: &str) -> Result<String> {
    let host = host.trim().trim_end_matches('.');
    if host.is_empty() {
        return Err(Error::HttpRequestError(
            "onion proxy target host must not be empty".to_string(),
        ));
    }
    if host.chars().any(char::is_whitespace) {
        return Err(Error::HttpRequestError(format!(
            "onion proxy target host {host:?} must not contain whitespace"
        )));
    }
    Ok(host.to_ascii_lowercase())
}

/// A proxy route selected for a target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionProxyRoute {
    /// Requested ingress protocol.
    pub protocol: OnionProxyProtocol,
    /// Target requested by the local client.
    pub target: OnionProxyTarget,
    /// Selected route ending at the exit.
    pub route: OnionRoute,
}

impl OnionProxyRoute {
    /// Return the selected exit DID.
    pub fn exit_did(&self) -> Did {
        self.route.exit_did()
    }

    /// Return the exit service used for route selection.
    pub const fn exit_service(&self) -> &'static str {
        self.protocol.exit_service()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_protocol_maps_to_exit_service() {
        assert_eq!(OnionProxyProtocol::TcpConnect.exit_service(), "tcp");
        assert_eq!(OnionProxyProtocol::HttpsFetch.exit_service(), "https");
    }

    #[test]
    fn target_authority_parses_domain_targets() -> Result<()> {
        let target = OnionProxyTarget::parse_authority("Example.COM.:443")?;

        assert_eq!(target.host(), "example.com");
        assert_eq!(target.port(), 443);
        assert_eq!(target.authority(), "example.com:443");
        Ok(())
    }

    #[test]
    fn target_authority_parses_ipv6_targets() -> Result<()> {
        let target = OnionProxyTarget::parse_authority("[2001:db8::1]:8443")?;

        assert_eq!(target.host(), "2001:db8::1");
        assert_eq!(target.port(), 8443);
        assert_eq!(target.authority(), "[2001:db8::1]:8443");
        Ok(())
    }

    #[test]
    fn target_authority_rejects_missing_port() {
        assert!(matches!(
            OnionProxyTarget::parse_authority("example.com"),
            Err(Error::HttpRequestError(_))
        ));
    }
}
