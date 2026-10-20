//! Client-side onion proxy planning.
//!
//! This module is runtime-neutral: native can bind it to a local HTTP CONNECT listener, while
//! browser callers can use the same target and service mapping before handing requests to a
//! browser-specific adapter. A proxy protocol is target-agnostic: it fixes the symbol, each
//! request supplies its own target authority, and the pipeline's loop shape fixes the route
//! length (#834 D5).

use rings_core::dht::Did;

use crate::onion::OnionExitDescriptor;
pub use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion::OnionServiceName;
use crate::online::OnlineNodeType;

#[cfg(rings_native)]
pub mod http;

/// Proxy protocol requested by the client ingress.
///
/// The protocol fixes the world-facing symbol, `TcpConnect ↦ tcp` and `HttpsProxy ↦ https`:
/// `https` is a request/response fetch only, so every byte tunnel, TLS included, is `tcp`, and
/// an operator who wants HTTPS-only egress registers `tcp` restricted to `*:443` (#834 D1′).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionProxyProtocol {
    /// HTTP CONNECT, SOCKS CONNECT, or any other byte tunnel. Requires a native `tcp` exit.
    TcpConnect,
    /// One HTTPS request/response exchange through an `https` exit.
    HttpsProxy,
}

impl OnionProxyProtocol {
    /// Return the world-facing symbol this protocol applies.
    pub const fn exit_service_name(self) -> OnionServiceName {
        match self {
            Self::TcpConnect => OnionServiceName::tcp(),
            Self::HttpsProxy => OnionServiceName::https(),
        }
    }

    /// Return a stable diagnostic label for this proxy protocol.
    pub const fn label(self) -> &'static str {
        match self {
            Self::TcpConnect => "tcp-connect",
            Self::HttpsProxy => "https-proxy",
        }
    }

    /// Whether `descriptor` registers this protocol's symbol, on a runtime able to interpret it:
    /// only native and FFI nodes have sockets, so only they serve `tcp`.
    pub(crate) fn accepts_exit_descriptor(self, descriptor: &OnionExitDescriptor) -> bool {
        let runtime_serves = match self {
            Self::TcpConnect => matches!(
                descriptor.node_type,
                OnlineNodeType::Native | OnlineNodeType::Ffi
            ),
            Self::HttpsProxy => true,
        };
        runtime_serves && descriptor.service == self.exit_service_name()
    }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::error::Result;

    /// The protocol alone fixes the symbol: a byte tunnel is always `tcp`, a fetch `https`.
    #[test]
    fn test_proxy_protocol_fixes_its_symbol() {
        assert_eq!(
            OnionProxyProtocol::TcpConnect.exit_service_name(),
            OnionServiceName::tcp()
        );
        assert_eq!(
            OnionProxyProtocol::HttpsProxy.exit_service_name(),
            OnionServiceName::https()
        );
    }

    #[test]
    fn test_target_authority_parses_domain_targets() -> Result<()> {
        let target = OnionProxyTarget::parse_authority("Example.COM.:443")?;

        assert_eq!(target.host(), "example.com");
        assert_eq!(target.port(), 443);
        assert_eq!(target.authority(), "example.com:443");
        Ok(())
    }

    #[test]
    fn test_target_authority_parses_ipv6_targets() -> Result<()> {
        let target = OnionProxyTarget::parse_authority("[2001:db8::1]:8443")?;

        assert_eq!(target.host(), "2001:db8::1");
        assert_eq!(target.port(), 8443);
        assert_eq!(target.authority(), "[2001:db8::1]:8443");
        Ok(())
    }

    #[test]
    fn test_target_authority_rejects_missing_port() {
        assert!(matches!(
            OnionProxyTarget::parse_authority("example.com"),
            Err(Error::OnionProxyTarget(
                crate::onion::OnionProxyTargetError::MissingPort
            ))
        ));
    }
}
