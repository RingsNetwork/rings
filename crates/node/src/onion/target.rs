//! Target authority parsing shared by onion policy and proxy adapters.

use std::net::IpAddr;
#[cfg(feature = "node")]
use std::net::SocketAddr;

#[cfg(feature = "node")]
use tokio::net::lookup_host;

use crate::error::Error;
use crate::error::Result;

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

/// Resolve an exit target exactly once and retain only publicly routable
/// addresses. Callers must connect to one of the returned socket addresses,
/// rather than resolving the hostname again, so a DNS rebinding cannot change
/// the destination after this admission decision.
#[cfg(feature = "node")]
pub(crate) async fn resolve_public_target(target: &OnionProxyTarget) -> Result<Vec<SocketAddr>> {
    let addresses = lookup_host((target.host(), target.port()))
        .await
        .map_err(|error| {
            Error::InvalidConfig(format!(
                "resolve onion exit target {:?}: {error}",
                target.authority()
            ))
        })?;
    let mut saw_address = false;
    let mut public_addresses = Vec::new();
    for address in addresses {
        saw_address = true;
        if is_public_exit_ip(address.ip()) && !public_addresses.contains(&address) {
            public_addresses.push(address);
        }
    }
    if !public_addresses.is_empty() {
        return Ok(public_addresses);
    }
    if saw_address {
        Err(Error::NoPermission)
    } else {
        Err(Error::InvalidConfig(format!(
            "onion exit target {:?} resolved empty",
            target.authority()
        )))
    }
}

/// Browser exits cannot pin a hostname to the address admitted by this node.
/// Only a public literal address is therefore safe in that environment.
#[cfg(all(not(feature = "node"), feature = "browser", target_family = "wasm"))]
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

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::is_public_exit_ip;
    #[cfg(feature = "node")]
    use super::resolve_public_target;
    #[cfg(feature = "node")]
    use super::OnionProxyTarget;
    #[cfg(feature = "node")]
    use crate::error::Error;

    #[test]
    fn exit_address_predicate_rejects_internal_and_special_destinations() {
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
    fn exit_address_predicate_accepts_public_destinations() {
        for address in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            let address = address.parse::<IpAddr>().expect("valid fixture address");
            assert!(
                is_public_exit_ip(address),
                "rejected public address {address}"
            );
        }
    }

    #[cfg(feature = "node")]
    #[tokio::test]
    async fn resolver_rejects_loopback_before_any_exit_connection() {
        let target =
            OnionProxyTarget::parse_authority("127.0.0.1:443").expect("valid loopback authority");

        assert!(matches!(
            resolve_public_target(&target).await,
            Err(Error::NoPermission)
        ));
    }
}
