#![warn(missing_docs)]
//! Client-side onion proxy planning.
//!
//! This module is runtime-neutral: native can bind it to a local HTTP CONNECT listener, while
//! browser callers can use the same target and service mapping before handing bytes to a
//! browser-specific HTTPS data plane. A proxy configuration is target-agnostic; each request
//! supplies its own target authority.

use rings_core::dht::Did;

use crate::error::Error;
use crate::error::Result;
use crate::onion::OnionExitService;
use crate::onion::OnionExitTransport;
pub use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion::OnionServiceName;

#[cfg(feature = "node")]
pub mod http;

/// Exit service used by native HTTP CONNECT/SOCKS-style byte tunnels.
pub const ONION_PROXY_TCP_SERVICE: &str = "tcp";

/// Exit service used by browser/application-layer HTTPS proxying.
pub const ONION_PROXY_HTTPS_SERVICE: &str = "https";

/// Proxy protocol requested by the client ingress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionProxyProtocol {
    /// HTTP CONNECT, SOCKS CONNECT, or any other byte tunnel. Requires a native TCP exit.
    TcpConnect,
    /// Application-layer HTTPS proxying. This is the browser-compatible mode.
    HttpsProxy,
}

impl OnionProxyProtocol {
    /// Return the onion-exit service name required by this proxy protocol.
    pub const fn exit_service(self) -> &'static str {
        match self {
            Self::TcpConnect => ONION_PROXY_TCP_SERVICE,
            Self::HttpsProxy => ONION_PROXY_HTTPS_SERVICE,
        }
    }

    /// Return the onion-exit transport required by this proxy protocol.
    pub const fn exit_transport(self) -> OnionExitTransport {
        match self {
            Self::TcpConnect => OnionExitTransport::Tcp,
            Self::HttpsProxy => OnionExitTransport::Https,
        }
    }

    fn default_exit_service_name(self) -> OnionServiceName {
        match self {
            Self::TcpConnect => OnionServiceName::tcp(),
            Self::HttpsProxy => OnionServiceName::https(),
        }
    }
}

/// Target-agnostic onion proxy configuration.
///
/// A client owns one proxy configuration per ingress style, then resolves one route per target
/// authority. This keeps browser proxy APIs from becoming one-off URL fetch wrappers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionProxyConfig {
    /// Requested ingress protocol.
    pub protocol: OnionProxyProtocol,
    service: OnionServiceName,
    /// Desired hop count including the exit. `0` uses [`crate::onion::DEFAULT_ONION_ROUTE_HOPS`].
    pub hop_count: usize,
    /// Whether route selection may use fewer hops when too few relays are live.
    pub allow_short_paths: bool,
}

impl OnionProxyConfig {
    /// Create a proxy configuration for `protocol`.
    pub fn new(protocol: OnionProxyProtocol, hop_count: usize, allow_short_paths: bool) -> Self {
        Self {
            protocol,
            service: protocol.default_exit_service_name(),
            hop_count,
            allow_short_paths,
        }
    }

    /// Create a proxy configuration with an explicit exit service.
    pub fn with_service(
        protocol: OnionProxyProtocol,
        service: OnionServiceName,
        hop_count: usize,
        allow_short_paths: bool,
    ) -> Result<Self> {
        validate_proxy_service(protocol, &service)?;
        Ok(Self {
            protocol,
            service,
            hop_count,
            allow_short_paths,
        })
    }

    /// Create a native TCP CONNECT proxy configuration.
    pub fn tcp_connect(hop_count: usize, allow_short_paths: bool) -> Self {
        Self::new(OnionProxyProtocol::TcpConnect, hop_count, allow_short_paths)
    }

    /// Create a native TCP CONNECT proxy configuration for a specific TCP exit service.
    pub fn tcp_connect_service(
        service: OnionServiceName,
        hop_count: usize,
        allow_short_paths: bool,
    ) -> Result<Self> {
        Self::with_service(
            OnionProxyProtocol::TcpConnect,
            service,
            hop_count,
            allow_short_paths,
        )
    }

    /// Create a browser-compatible HTTPS proxy configuration.
    pub fn https_proxy(hop_count: usize, allow_short_paths: bool) -> Self {
        Self::new(OnionProxyProtocol::HttpsProxy, hop_count, allow_short_paths)
    }

    /// Return the onion-exit service name required by this proxy.
    pub fn exit_service(&self) -> &str {
        self.service.as_str()
    }

    /// Return the canonical onion-exit service required by this proxy.
    pub fn exit_service_name(&self) -> &OnionServiceName {
        &self.service
    }

    /// Return the onion-exit transport required by this proxy.
    pub fn exit_transport(&self) -> OnionExitTransport {
        self.protocol.exit_transport()
    }
}

fn validate_proxy_service(protocol: OnionProxyProtocol, service: &OnionServiceName) -> Result<()> {
    if let Some(expected) = OnionExitService::reserved_transport(service.as_str()) {
        if expected != protocol.exit_transport() {
            return Err(Error::InvalidConfig(format!(
                "onion proxy service {:?} requires {:?} transport, got {:?}",
                service.as_str(),
                expected,
                protocol.exit_transport()
            )));
        }
    }
    Ok(())
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
    pub fn exit_service(&self) -> &str {
        self.route.service()
    }

    /// Return the exit transport used for route selection.
    pub const fn exit_transport(&self) -> OnionExitTransport {
        self.protocol.exit_transport()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::error::Result;

    #[test]
    fn proxy_protocol_maps_to_exit_service() {
        assert_eq!(OnionProxyProtocol::TcpConnect.exit_service(), "tcp");
        assert_eq!(OnionProxyProtocol::HttpsProxy.exit_service(), "https");
        assert_eq!(
            OnionProxyProtocol::TcpConnect.exit_transport(),
            OnionExitTransport::Tcp
        );
        assert_eq!(
            OnionProxyProtocol::HttpsProxy.exit_transport(),
            OnionExitTransport::Https
        );
    }

    #[test]
    fn proxy_config_is_target_agnostic() {
        let proxy = OnionProxyConfig::https_proxy(3, false);

        assert_eq!(proxy.exit_service(), "https");
        assert_eq!(proxy.exit_transport(), OnionExitTransport::Https);
        assert_eq!(proxy.hop_count, 3);
        assert!(!proxy.allow_short_paths);
    }

    #[test]
    fn tcp_proxy_config_accepts_custom_tcp_service() -> Result<()> {
        let service = OnionServiceName::parse("web")?;
        let proxy = OnionProxyConfig::tcp_connect_service(service, 2, true)?;

        assert_eq!(proxy.exit_service(), "web");
        assert_eq!(proxy.exit_transport(), OnionExitTransport::Tcp);
        assert_eq!(proxy.hop_count, 2);
        assert!(proxy.allow_short_paths);
        Ok(())
    }

    #[test]
    fn proxy_config_rejects_reserved_service_transport_mismatch() {
        assert!(matches!(
            OnionProxyConfig::tcp_connect_service(OnionServiceName::https(), 1, false),
            Err(Error::InvalidConfig(_))
        ));
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
