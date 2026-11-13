//! Target authority parsing shared by onion policy and proxy adapters.

#[cfg(any(test, rings_native))]
use std::net::SocketAddr;

#[cfg(rings_native)]
use tokio::net::lookup_host;
use url::Host;
#[cfg(any(all(feature = "browser", target_family = "wasm"), test))]
use url::Url;

use crate::error::Error;
use crate::error::Result;

/// Closed parse failures for an onion proxy authority.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OnionProxyTargetError {
    /// Authority is empty after trimming.
    #[error("onion proxy target authority must not be empty")]
    EmptyAuthority,
    /// A bracketed IPv6 host has no closing bracket.
    #[error("invalid bracketed IPv6 onion proxy authority")]
    MissingIpv6Bracket,
    /// The authority has no explicit port separator or port value.
    #[error("onion proxy authority must include a port")]
    MissingPort,
    /// Host is empty after canonicalization.
    #[error("onion proxy target host must not be empty")]
    EmptyHost,
    /// Host contains whitespace.
    #[error("onion proxy target host must not contain whitespace")]
    HostWhitespace,
    /// Host contains `*`: no authority has a wildcard host, so a `*` is never a literal (the
    /// exit patterns `*`, `*:*` and `*:port` are recognised before any host is parsed).
    #[error("onion proxy target host must not contain '*'")]
    WildcardHost,
    /// Host is not a syntactically valid domain or IP literal.
    #[error("onion proxy target host is invalid")]
    InvalidHost,
    /// Port is not a valid `u16`.
    #[error("onion proxy target has an invalid port")]
    InvalidPort,
    /// Port zero cannot identify a connect target.
    #[error("onion proxy target port must be non-zero")]
    ZeroPort,
}

/// Host/port target requested through an onion proxy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionProxyTarget {
    host: String,
    port: u16,
}

impl OnionProxyTarget {
    /// Validate a host and port already separated by a URL or proxy parser.
    pub fn new(host: &str, port: u16) -> Result<Self> {
        let host = normalize_host(host)?;
        if port == 0 {
            return Err(OnionProxyTargetError::ZeroPort.into());
        }
        Ok(Self { host, port })
    }

    /// Parse an HTTP CONNECT authority (`host:port` or `[ipv6]:port`).
    pub fn parse_authority(authority: &str) -> Result<Self> {
        let authority = authority.trim();
        if authority.is_empty() {
            return Err(OnionProxyTargetError::EmptyAuthority.into());
        }

        let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
            let Some((host, rest)) = rest.split_once(']') else {
                return Err(OnionProxyTargetError::MissingIpv6Bracket.into());
            };
            let Some(port) = rest.strip_prefix(':') else {
                return Err(OnionProxyTargetError::MissingPort.into());
            };
            (host, port)
        } else {
            authority
                .rsplit_once(':')
                .ok_or(OnionProxyTargetError::MissingPort)?
        };

        let port = port
            .parse::<u16>()
            .map_err(|_| OnionProxyTargetError::InvalidPort)?;
        Self::new(host, port)
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

/// Resolve an exit target exactly once and retain only publicly routable
/// addresses. Callers must connect to one of the returned socket addresses,
/// rather than resolving the hostname again, so a DNS rebinding cannot change
/// the destination after this admission decision.
#[cfg(rings_native)]
pub(crate) async fn resolve_public_target(target: &OnionProxyTarget) -> Result<Vec<SocketAddr>> {
    let addresses = resolve_target_addresses(target).await?;
    match select_public_exit_addresses(addresses) {
        PublicAddressSelection::Public(addresses) => Ok(addresses),
        PublicAddressSelection::Denied => Err(Error::NoPermission),
        PublicAddressSelection::Empty => Err(Error::OnionTargetResolvedEmpty {
            authority: target.authority(),
        }),
    }
}

/// Resolve one target into a single DNS snapshot without assigning egress authority:
/// [`resolve_public_target`] passes the complete snapshot through
/// [`select_public_exit_addresses`] before any address is used.
#[cfg(rings_native)]
async fn resolve_target_addresses(target: &OnionProxyTarget) -> Result<Vec<SocketAddr>> {
    Ok(lookup_host((target.host(), target.port()))
        .await
        .map_err(|error| Error::OnionTargetResolve {
            authority: target.authority(),
            source: error,
        })?
        .collect())
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(any(test, rings_native))]
enum PublicAddressSelection {
    Empty,
    Denied,
    Public(Vec<SocketAddr>),
}

/// Select a stable, de-duplicated public projection from one DNS result snapshot.
#[cfg(any(test, rings_native))]
fn select_public_exit_addresses(addresses: Vec<SocketAddr>) -> PublicAddressSelection {
    if addresses.is_empty() {
        return PublicAddressSelection::Empty;
    }
    let public = addresses
        .into_iter()
        .filter(|address| rings_network_policy::is_public_ip(address.ip()))
        .fold(Vec::new(), |mut selected, address| {
            if !selected.contains(&address) {
                selected.push(address);
            }
            selected
        });
    if public.is_empty() {
        PublicAddressSelection::Denied
    } else {
        PublicAddressSelection::Public(public)
    }
}

/// Validate that `target`'s host is an explicit public IPv4 or IPv6 literal.
///
/// Browser `fetch()` cannot pin resolved DNS addresses before connecting, so browser HTTPS exits
/// only admit public IP-literal destinations.
pub(crate) fn validate_public_ip_literal(target: &OnionProxyTarget) -> Result<()> {
    let ip = target
        .host()
        .parse::<std::net::IpAddr>()
        .map_err(|_| Error::NoPermission)?;
    if rings_network_policy::is_public_ip(ip) {
        Ok(())
    } else {
        Err(Error::NoPermission)
    }
}

/// Validate that an HTTPS exit request URL parses as an `https://` target without embedded
/// credentials, targets an explicit public IP literal, and matches `target`'s canonical host and
/// port.
#[cfg(any(all(feature = "browser", target_family = "wasm"), test))]
pub(crate) fn validate_public_https_url(url: &str, target: &OnionProxyTarget) -> Result<()> {
    validate_public_ip_literal(target)?;
    let parsed = Url::parse(url).map_err(|_| Error::NoPermission)?;
    if parsed.scheme() != "https" || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::NoPermission);
    }
    if parsed.port_or_known_default() != Some(target.port()) {
        return Err(Error::NoPermission);
    }
    let parsed_host = parsed.host_str().ok_or(Error::NoPermission)?;
    let parsed_target =
        OnionProxyTarget::new(parsed_host, target.port()).map_err(|_| Error::NoPermission)?;
    validate_public_ip_literal(&parsed_target)?;
    if parsed_target != *target {
        return Err(Error::NoPermission);
    }
    Ok(())
}

/// The canonical host: trimmed, without trailing dots, canonicalized via `url::Host::parse`
/// (IDNA Punycode lowercase ASCII for domains, canonical dotted-quad / compressed IPv6 for IP
/// literals); empty, whitespace, `*`, and syntactically invalid hosts are refused.
fn normalize_host(host: &str) -> Result<String> {
    let host = host.trim().trim_end_matches('.');
    if host.is_empty() {
        return Err(OnionProxyTargetError::EmptyHost.into());
    }
    if host.chars().any(char::is_whitespace) {
        return Err(OnionProxyTargetError::HostWhitespace.into());
    }
    if host.contains('*') {
        return Err(OnionProxyTargetError::WildcardHost.into());
    }
    let parsed = if host.contains(':') && !host.starts_with('[') {
        Host::parse(&format!("[{host}]"))
    } else {
        Host::parse(host)
    }
    .map_err(|_| OnionProxyTargetError::InvalidHost)?;
    Ok(match parsed {
        Host::Domain(domain) => domain,
        Host::Ipv4(ipv4) => ipv4.to_string(),
        Host::Ipv6(ipv6) => ipv6.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::net::SocketAddr;

    #[cfg(rings_native)]
    use super::resolve_public_target;
    use super::select_public_exit_addresses;
    #[cfg(rings_native)]
    use super::OnionProxyTarget;
    use super::PublicAddressSelection;
    #[cfg(rings_native)]
    use crate::error::Error;

    #[test]
    fn test_public_address_selection_distinguishes_empty_denied_and_deduplicated_public() {
        let denied: SocketAddr = "127.0.0.1:443".parse().expect("denied address");
        let public: SocketAddr = "8.8.8.8:443".parse().expect("public address");

        assert_eq!(
            select_public_exit_addresses(Vec::new()),
            PublicAddressSelection::Empty
        );
        assert_eq!(
            select_public_exit_addresses(vec![denied]),
            PublicAddressSelection::Denied
        );
        assert_eq!(
            select_public_exit_addresses(vec![denied, public, public]),
            PublicAddressSelection::Public(vec![public])
        );
    }

    #[test]
    fn test_exit_address_predicate_rejects_internal_and_special_destinations() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.0.1",
            "198.18.0.1",
            "224.0.0.1",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::7f00:1",
            "2001:db8::1",
            "2002:7f00:1::",
            "3fff::1",
            "4000::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
        ] {
            let address = address.parse::<IpAddr>().expect("valid fixture address");
            assert!(
                !rings_network_policy::is_public_ip(address),
                "accepted special address {address}"
            );
        }
    }

    #[test]
    fn test_exit_address_predicate_accepts_public_destinations() {
        for address in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            let address = address.parse::<IpAddr>().expect("valid fixture address");
            assert!(
                rings_network_policy::is_public_ip(address),
                "rejected public address {address}"
            );
        }
    }

    #[test]
    fn test_idna_punycode_canonicalization_and_public_https_url_validation() {
        let idna = super::OnionProxyTarget::parse_authority("Bücher.Example.:443")
            .expect("valid IDNA authority");
        assert_eq!(idna.host(), "xn--bcher-kva.example");
        assert_eq!(idna.authority(), "xn--bcher-kva.example:443");

        let public_ip = super::OnionProxyTarget::parse_authority("1.1.1.1:443").expect("public ip");
        assert!(super::validate_public_https_url("https://1.1.1.1/dns-query", &public_ip).is_ok());
        let public_ipv6 =
            super::OnionProxyTarget::parse_authority("[2606:4700:4700::1111]:443").expect("ipv6");
        assert!(super::validate_public_https_url(
            "https://[2606:4700:4700::1111]:443/dns-query",
            &public_ipv6,
        )
        .is_ok());

        for denied_authority in [
            "xn--bcher-kva.example:443",
            "service.example.com:443",
            "127.0.0.1:443",
            "10.0.0.1:443",
            "[::1]:443",
            "localhost:443",
            "service.local:443",
            "metadata.google.internal:443",
        ] {
            let target =
                super::OnionProxyTarget::parse_authority(denied_authority).expect("authority");
            let url = format!("https://{}/", target.authority());
            assert!(
                matches!(
                    super::validate_public_https_url(&url, &target),
                    Err(crate::error::Error::NoPermission)
                ),
                "unexpectedly allowed {url}"
            );
        }
    }

    #[cfg(rings_native)]
    #[tokio::test]
    async fn test_resolver_rejects_loopback_before_any_exit_connection() {
        let target =
            OnionProxyTarget::parse_authority("127.0.0.1:443").expect("valid loopback authority");

        assert!(matches!(
            resolve_public_target(&target).await,
            Err(Error::NoPermission)
        ));
    }
}
