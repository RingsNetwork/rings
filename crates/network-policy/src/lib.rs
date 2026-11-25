#![deny(missing_docs)]
//! Shared fail-closed admission predicates for network egress targets.
//!
//! URL admission and DNS resolution are separate proof obligations. This crate rejects
//! non-public IP literals and local-name forms before any transport runs. Native transports must
//! additionally resolve a hostname once, retain only addresses accepted by [`is_public_ip`], and
//! connect to that retained snapshot so DNS rebinding cannot change the destination.

use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;

use url::Host;
use url::Url;

/// Closed reasons that a URL host is ineligible for public-network egress.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PublicHostError {
    /// The URL has no host component.
    #[error("public network target must include a host")]
    MissingHost,
    /// An IP literal belongs to a non-public or special-purpose range.
    #[error("target IP address {0} is not publicly routable")]
    NonPublicIp(IpAddr),
    /// A domain is a local or ambiguous internal name rather than a public DNS name.
    #[error("target host {0:?} is a local or ambiguous internal name")]
    LocalDomain(String),
}

/// Validate the host portion of an absolute URL before public-network egress.
///
/// This is a transport-independent first gate. A native transport that accepts a domain must
/// still resolve it once, apply [`is_public_ip`] to every candidate, and pin the admitted result.
pub fn validate_public_url_host(url: &Url) -> Result<(), PublicHostError> {
    match url.host().ok_or(PublicHostError::MissingHost)? {
        Host::Ipv4(address) => validate_public_ip(IpAddr::V4(address)),
        Host::Ipv6(address) => validate_public_ip(IpAddr::V6(address)),
        Host::Domain(domain) => validate_public_domain(domain),
    }
}

fn validate_public_ip(address: IpAddr) -> Result<(), PublicHostError> {
    if is_public_ip(address) {
        Ok(())
    } else {
        Err(PublicHostError::NonPublicIp(address))
    }
}

fn validate_public_domain(domain: &str) -> Result<(), PublicHostError> {
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    let is_single_label = !domain.contains('.');
    let has_local_suffix = ["localhost", "local", "internal", "lan", "home", "home.arpa"]
        .into_iter()
        .any(|suffix| domain == suffix || domain.ends_with(&format!(".{suffix}")));
    if domain.is_empty() || is_single_label || has_local_suffix {
        Err(PublicHostError::LocalDomain(domain))
    } else {
        Ok(())
    }
}

/// Return whether an address is eligible for arbitrary public-network egress.
///
/// The predicate is intentionally conservative. Private, loopback, link-local, carrier-grade
/// NAT, documentation, benchmarking, multicast, reserved, transition, and translation prefixes
/// are rejected. IPv4 values embedded in IPv6 are classified by their IPv4 address.
pub const fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address.octets()),
        IpAddr::V6(address) => {
            let octets = address.octets();
            if let Some(ipv4) = embedded_ipv4(octets) {
                return is_public_ipv4(ipv4);
            }
            // Public IPv6 unicast is currently allocated only from 2000::/3. Rejecting every
            // other top-level block fails closed as IANA assigns new space.
            if octets[0] < 0x20 || octets[0] > 0x3f {
                return false;
            }
            !matches!(
                octets,
                // IETF protocol assignments (2001:0000::/23), including Teredo (2001:0000::/32),
                // Benchmarking (2001:0002::/48), and ORCHID (2001:0010::/28, 2001:0020::/28).
                [0x20, 0x01, 0x00..=0x01, ..]
                    // Documentation 2001:db8::/32.
                    | [0x20, 0x01, 0x0d, 0xb8, ..]
                    // Documentation 3fff::/20 (RFC 9637).
                    | [0x3f, 0xff, 0x00..=0x0f, ..]
            )
        }
    }
}

/// Return whether an IP address is a valid unicast destination when local and private networks are
/// permitted (i.e. not unspecified, multicast, broadcast, or Class E reserved).
pub const fn is_valid_local_or_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let first = address.octets()[0];
            first != 0 && first < 224
        }
        IpAddr::V6(address) => {
            if address.is_unspecified() || address.is_multicast() {
                return false;
            }
            if address.is_loopback() {
                return true;
            }
            if let Some([first, ..]) = embedded_ipv4(address.octets()) {
                return first != 0 && first < 224;
            }
            true
        }
    }
}

/// Return whether an IPv4 address is globally routable public unicast.
pub const fn is_public_ipv4([first, second, third, _fourth]: [u8; 4]) -> bool {
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
        return Some([octets[12], octets[13], octets[14], octets[15]]);
    }
    // Well-Known NAT64 prefix 64:ff9b::/96 (RFC 6052) embeds IPv4 in octets 12..16.
    if octets[0] == 0x00
        && octets[1] == 0x64
        && octets[2] == 0xff
        && octets[3] == 0x9b
        && octets[4] == 0
        && octets[5] == 0
        && octets[6] == 0
        && octets[7] == 0
        && octets[8] == 0
        && octets[9] == 0
        && octets[10] == 0
        && octets[11] == 0
    {
        return Some([octets[12], octets[13], octets[14], octets[15]]);
    }
    // 6to4 prefix 2002::/16 (RFC 3056) embeds IPv4 in octets 2..6.
    if octets[0] == 0x20 && octets[1] == 0x02 {
        return Some([octets[2], octets[3], octets[4], octets[5]]);
    }
    None
}

const fn is_forbidden_turn_tcp_ipv4(v4: Ipv4Addr, host_is_explicit_loopback: bool) -> bool {
    let [first, ..] = v4.octets();
    v4.is_unspecified()
        || v4.is_multicast()
        || v4.is_broadcast()
        || v4.is_link_local()
        || first == 0
        || first >= 224
        || (v4.is_loopback() && !host_is_explicit_loopback)
}

/// Return whether dialing `addr` resolved for TURN-over-TCP `host` violates egress policy.
///
/// Rejects port `0`, unspecified/reserved (`0.0.0.0/8`), multicast/Class E (`>= 224.0.0.0`),
/// broadcast, link-local (`169.254.0.0/16`, `fe80::/10`), and loopback targets — including IPv4
/// addresses embedded inside IPv4-compatible (`::/96`), IPv4-mapped (`::ffff:0:0/96`), NAT64
/// (`64:ff9b::/96`), and 6to4 (`2002::/16`) IPv6 prefixes — unless `host` explicitly names
/// `localhost` or a loopback IP literal.
pub fn is_forbidden_turn_tcp_addr(host: &str, addr: SocketAddr) -> bool {
    if addr.port() == 0 {
        return true;
    }
    let host_ip = host
        .trim_matches(|c| c == '[' || c == ']')
        .parse::<IpAddr>()
        .ok();
    let host_is_explicit_loopback =
        host.eq_ignore_ascii_case("localhost") || host_ip.is_some_and(|ip| ip.is_loopback());

    match addr.ip() {
        IpAddr::V4(v4) => is_forbidden_turn_tcp_ipv4(v4, host_is_explicit_loopback),
        IpAddr::V6(v6) => {
            if v6.is_unspecified() || v6.is_multicast() || (v6.segments()[0] & 0xffc0) == 0xfe80 {
                return true;
            }
            if v6.is_loopback() {
                return !host_is_explicit_loopback;
            }
            if let Some(octets) = embedded_ipv4(v6.octets()) {
                return is_forbidden_turn_tcp_ipv4(
                    Ipv4Addr::from(octets),
                    host_is_explicit_loopback,
                );
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_public_and_embedded_addresses_are_rejected() {
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
            let parsed = address.parse::<IpAddr>();
            assert!(matches!(parsed, Ok(address) if !is_public_ip(address)));
        }
    }

    #[test]
    fn public_addresses_are_accepted() {
        for address in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            let parsed = address.parse::<IpAddr>();
            assert!(matches!(parsed, Ok(address) if is_public_ip(address)));
        }
        for address in ["::1", "127.0.0.1"] {
            let parsed = address.parse::<IpAddr>();
            assert!(matches!(
                parsed,
                Ok(address) if is_valid_local_or_public_ip(address)
            ));
        }
        assert!(matches!(
            "::".parse::<IpAddr>(),
            Ok(address) if !is_valid_local_or_public_ip(address)
        ));
    }

    #[test]
    fn url_host_policy_rejects_local_literals_and_names() {
        for target in [
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://169.254.169.254/latest/meta-data/",
            "https://localhost/",
            "https://api.localhost/",
            "https://printer.local/",
            "https://metadata.google.internal/",
            "https://intranet/",
        ] {
            let parsed = Url::parse(target);
            assert!(matches!(
                parsed.map(|url| validate_public_url_host(&url)),
                Ok(Err(_))
            ));
        }
    }

    #[test]
    fn url_host_policy_accepts_public_literals_and_domains() {
        for target in [
            "https://1.1.1.1/",
            "https://[2606:4700:4700::1111]/",
            "http://example.com/",
            "https://api.example.test/",
        ] {
            let parsed = Url::parse(target);
            assert!(matches!(
                parsed.map(|url| validate_public_url_host(&url)),
                Ok(Ok(()))
            ));
        }
    }

    #[test]
    fn turn_tcp_egress_rejects_embedded_ipv4_ssrf_and_port_zero(
    ) -> Result<(), std::net::AddrParseError> {
        let forbidden_for_domain = [
            "0.0.0.0:3478",
            "0.1.2.3:3478",
            "127.0.0.1:3478",
            "169.254.169.254:3478",
            "224.0.0.1:3478",
            "255.255.255.255:3478",
            "1.1.1.1:0",
            "[::]:3478",
            "[::1]:3478",
            "[fe80::1]:3478",
            "[ff02::1]:3478",
            "[::127.0.0.1]:3478",
            "[::169.254.169.254]:3478",
            "[::ffff:127.0.0.1]:3478",
            "[::ffff:169.254.169.254]:3478",
            "[64:ff9b::127.0.0.1]:3478",
            "[64:ff9b::169.254.169.254]:3478",
            "[2002:7f00:0001::1]:3478",
            "[2002:a9fe:a9fe::1]:3478",
        ];
        for raw in forbidden_for_domain {
            let addr: SocketAddr = raw.parse()?;
            assert!(
                is_forbidden_turn_tcp_addr("turn.rings.example", addr),
                "expected {raw} to be rejected for public domain host"
            );
        }

        // Explicit loopback hosts permit local test/loopback targets, still forbid link-local/port 0.
        assert!(!is_forbidden_turn_tcp_addr(
            "localhost",
            "127.0.0.1:3478".parse()?
        ));
        assert!(!is_forbidden_turn_tcp_addr(
            "127.0.0.1",
            "127.0.0.1:3478".parse()?
        ));
        assert!(!is_forbidden_turn_tcp_addr("[::1]", "[::1]:3478".parse()?));
        assert!(!is_forbidden_turn_tcp_addr(
            "localhost",
            "[::1]:3478".parse()?
        ));
        assert!(is_forbidden_turn_tcp_addr(
            "localhost",
            "[64:ff9b::169.254.169.254]:3478".parse()?
        ));
        assert!(!is_forbidden_turn_tcp_addr(
            "turn.rings.example",
            "1.1.1.1:3478".parse()?
        ));
        assert!(!is_forbidden_turn_tcp_addr(
            "turn.rings.example",
            "[2606:4700:4700::1111]:3478".parse()?
        ));
        Ok(())
    }
}
