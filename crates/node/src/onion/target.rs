//! Target authority parsing shared by onion policy and proxy adapters.

use std::net::IpAddr;
#[cfg(any(test, rings_native))]
use std::net::SocketAddr;

#[cfg(rings_native)]
use tokio::net::lookup_host;

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

        let host = normalize_host(host)?;
        let port = port
            .parse::<u16>()
            .map_err(|_| OnionProxyTargetError::InvalidPort)?;
        if port == 0 {
            return Err(OnionProxyTargetError::ZeroPort.into());
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

/// Resolve one target into a single DNS snapshot without assigning egress authority.
///
/// Callers must pass the complete snapshot through a closed address-selection policy before using
/// it. Keeping resolution separate lets an explicitly configured proxy recognize its synthetic
/// DNS placeholders without treating ordinary private or loopback results as public.
#[cfg(rings_native)]
pub(crate) async fn resolve_target_addresses(target: &OnionProxyTarget) -> Result<Vec<SocketAddr>> {
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
pub(crate) enum PublicAddressSelection {
    Empty,
    Denied,
    Public(Vec<SocketAddr>),
}

/// Select a stable, de-duplicated public projection from one DNS result snapshot.
#[cfg(any(test, rings_native))]
pub(crate) fn select_public_exit_addresses(addresses: Vec<SocketAddr>) -> PublicAddressSelection {
    if addresses.is_empty() {
        return PublicAddressSelection::Empty;
    }
    let public = addresses
        .into_iter()
        .filter(|address| is_public_exit_ip(address.ip()))
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

/// Browser exits cannot pin a hostname to the address admitted by this node.
/// Only a public literal address is therefore safe in that environment.
#[cfg(rings_browser)]
pub(crate) fn validate_public_ip_literal(target: &OnionProxyTarget) -> Result<()> {
    let address = target
        .host()
        .parse::<IpAddr>()
        .map_err(|_| Error::NoPermission)?;
    if is_public_exit_ip(address) {
        Ok(())
    } else {
        Err(Error::NoPermission)
    }
}

/// Return whether an address is eligible for arbitrary onion-exit egress.
///
/// This predicate is intentionally conservative: private, loopback, link-local,
/// carrier-grade NAT, documentation, benchmarking, multicast, reserved,
/// transition, and address-translation prefixes are all rejected. IPv4 values
/// embedded in IPv6 are classified by their IPv4 address.
const fn is_public_exit_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_exit_ipv4(address.octets()),
        IpAddr::V6(address) => {
            let octets = address.octets();
            if let Some(ipv4) = embedded_ipv4(octets) {
                return is_public_exit_ipv4(ipv4);
            }
            // Public IPv6 unicast is currently allocated only from 2000::/3.
            // Rejecting every other top-level block fails closed as IANA assigns
            // new space, rather than treating reserved space as public egress.
            if octets[0] < 0x20 || octets[0] > 0x3f {
                return false;
            }
            !matches!(
                octets,
                // IETF protocol assignments, including Teredo and ORCHID.
                [0x20, 0x01, 0x00..=0x01, ..]
                    // Documentation 2001:db8::/32.
                    | [0x20, 0x01, 0x0d, 0xb8, ..]
                    // 6to4 can embed a private IPv4 destination.
                    | [0x20, 0x02, ..]
                    // Documentation 3fff::/20.
                    | [0x3f, 0xf0..=0xff, ..]
            )
        }
    }
}

const fn embedded_ipv4(octets: [u8; 16]) -> Option<[u8; 4]> {
    let compatible_prefix = octets[0] == 0
        && octets[1] == 0
        && octets[2] == 0
        && octets[3] == 0
        && octets[4] == 0
        && octets[5] == 0
        && octets[6] == 0
        && octets[7] == 0
        && octets[8] == 0
        && octets[9] == 0;
    if compatible_prefix
        && ((octets[10] == 0 && octets[11] == 0) || (octets[10] == 0xff && octets[11] == 0xff))
    {
        Some([octets[12], octets[13], octets[14], octets[15]])
    } else {
        None
    }
}

const fn is_public_exit_ipv4([first, second, third, _fourth]: [u8; 4]) -> bool {
    !matches!(
        (first, second, third),
        (0, _, _)
            | (10, _, _)
            | (100, 64..=127, _)
            | (127, _, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 0, 0)
            | (192, 0, 2)
            | (192, 88, 99)
            | (192, 168, _)
            | (198, 18..=19, _)
            | (198, 51, 100)
            | (203, 0, 113)
            | (224..=255, _, _)
    )
}

fn normalize_host(host: &str) -> Result<String> {
    let host = host.trim().trim_end_matches('.');
    if host.is_empty() {
        return Err(OnionProxyTargetError::EmptyHost.into());
    }
    if host.chars().any(char::is_whitespace) {
        return Err(OnionProxyTargetError::HostWhitespace.into());
    }
    Ok(host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::net::SocketAddr;

    use super::is_public_exit_ip;
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
                !is_public_exit_ip(address),
                "accepted special address {address}"
            );
        }
    }

    #[test]
    fn test_exit_address_predicate_accepts_public_destinations() {
        for address in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            let address = address.parse::<IpAddr>().expect("valid fixture address");
            assert!(
                is_public_exit_ip(address),
                "rejected public address {address}"
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
