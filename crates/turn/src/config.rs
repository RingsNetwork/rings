use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rings_core::ecc::SecretKey;
use rings_core::utils::SharedClock;
use rings_core::utils::SystemClock;

use crate::auth::TurnAdmissionPolicy;
use crate::error::Result;
use crate::error::TurnError;
use crate::relay_backend::TurnDatagramRelayFactory;

/// Default TURN UDP listening port (RFC 5766).
pub const DEFAULT_TURN_PORT: u16 = 3478;

/// Default TURN authentication realm.
pub const DEFAULT_TURN_REALM: &str = "rings";

/// Default maximum retries when allocating a random relay port in a configured range.
pub const DEFAULT_PORT_RANGE_MAX_RETRIES: u16 = 32;

/// Default maximum number of active TURN nonce challenges stored concurrently.
pub const DEFAULT_MAX_NONCES: usize = 4096;

/// Default maximum number of active TURN relay allocations stored concurrently.
pub const DEFAULT_MAX_ALLOCATIONS: usize = 1024;

/// Default maximum number of unclaimed [`crate::SignedTurnSessionTicket`] reservations stored
/// concurrently per server (#1292).
pub const DEFAULT_MAX_RESERVATIONS: usize = 256;

/// Default maximum lifetime of an unclaimed [`crate::SignedTurnSessionTicket`] reservation (`60s`).
///
/// Bounding unclaimed ticket lifetime independently of `MAXIMUM_ALLOCATION_LIFETIME` (`3600s`)
/// ensures anonymous `turn.ticket` requests cannot hold reservation slots past a brief handshake
/// window.
pub const DEFAULT_MAX_RESERVATION_TTL: Duration = Duration::from_secs(60);

/// Default maximum number of active peer IP permissions per TURN allocation.
pub const DEFAULT_MAX_PERMISSIONS_PER_ALLOCATION: usize = 64;

/// Destination IP policy enforced on TURN relay permissions, channel bindings, and datagrams.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RelayPeerPolicy {
    /// Only allow relaying to globally routable public unicast IP addresses (default).
    ///
    /// Denies loopback (`127.0.0.0/8`, `::1`), unspecified (`0.0.0.0/8`, `::`), private RFC 1918
    /// (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`), shared CGNAT RFC 6598 (`100.64.0.0/10`),
    /// link-local (`169.254.0.0/16`, `fe80::/10`), IPv6 Unique Local (`fc00::/7`), multicast,
    /// broadcast, documentation/benchmarking/reserved blocks, IPv4-mapped/translated IPv6
    /// addresses wrapping any non-public IPv4 address, and port `0`.
    #[default]
    PublicOnly,
    /// Allow relaying to loopback, link-local, and private network addresses in addition to public
    /// unicast addresses (for local development and LAN testing).
    ///
    /// Still denies unspecified (`0.0.0.0/8`, `::`), multicast, broadcast, and port `0`.
    AllowLocalAndPrivate,
}

impl RelayPeerPolicy {
    /// Return whether `addr` is permitted as a TURN peer destination under this policy.
    pub const fn is_allowed_addr(self, addr: SocketAddr) -> bool {
        if addr.port() == 0 {
            return false;
        }
        self.is_allowed_ip(addr.ip())
    }

    /// Return whether `ip` is permitted as a TURN peer IP address under this policy.
    pub const fn is_allowed_ip(self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => self.is_allowed_ipv4(v4),
            IpAddr::V6(v6) => self.is_allowed_ipv6(v6),
        }
    }

    const fn is_allowed_ipv4(self, v4: Ipv4Addr) -> bool {
        let octets = v4.octets();
        // 0.0.0.0/8 ("this" network / unspecified), 224.0.0.0/4 (multicast),
        // 240.0.0.0/4 (reserved / Class E / broadcast 255.255.255.255).
        if octets[0] == 0 || octets[0] >= 224 {
            return false;
        }
        match self {
            Self::AllowLocalAndPrivate => true,
            Self::PublicOnly => {
                if v4.is_loopback() || v4.is_private() || v4.is_link_local() {
                    return false;
                }
                // 100.64.0.0/10 (Shared Address Space / CGNAT, RFC 6598)
                if octets[0] == 100 && (octets[1] & 0b1100_0000) == 64 {
                    return false;
                }
                // 192.0.0.0/24 (IETF Protocol Assignments, RFC 6890)
                if octets[0] == 192 && octets[1] == 0 && octets[2] == 0 {
                    return false;
                }
                // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24 (TEST-NET documentation, RFC 5737)
                // and 192.88.99.0/24 (deprecated 6to4 relay anycast, RFC 7526).
                if (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
                    || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
                    || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
                    || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
                {
                    return false;
                }
                // 198.18.0.0/15 (Benchmarking, RFC 2544)
                if octets[0] == 198 && (octets[1] & 0xfe) == 18 {
                    return false;
                }
                true
            }
        }
    }

    const fn is_allowed_ipv6(self, v6: Ipv6Addr) -> bool {
        if v6.is_unspecified() || v6.is_multicast() {
            return false;
        }
        if let Some(mapped_v4) = v6.to_ipv4_mapped() {
            return self.is_allowed_ipv4(mapped_v4);
        }
        let segments = v6.segments();
        let octets = v6.octets();
        // IPv4-compatible `::a.b.c.d` or Well-Known NAT64 `64:ff9b::/96` (RFC 6052)
        let is_v4_compat = segments[0] == 0
            && segments[1] == 0
            && segments[2] == 0
            && segments[3] == 0
            && segments[4] == 0
            && segments[5] == 0
            && !v6.is_loopback();
        let is_well_known_nat64 = segments[0] == 0x0064
            && segments[1] == 0xff9b
            && segments[2] == 0
            && segments[3] == 0
            && segments[4] == 0
            && segments[5] == 0;
        if is_v4_compat || is_well_known_nat64 {
            let embedded_v4 = Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15]);
            return self.is_allowed_ipv4(embedded_v4);
        }
        // Any other address under 64:ff9b::/32 (including the entire 64:ff9b:1::/48 local-use
        // translation block, RFC 8215, whose suffix layout is deployment-defined) is local-use.
        if segments[0] == 0x0064 && segments[1] == 0xff9b {
            return matches!(self, Self::AllowLocalAndPrivate);
        }

        match self {
            Self::AllowLocalAndPrivate => true,
            Self::PublicOnly => {
                // Only Global Unicast 2000::/3 (RFC 4291) is routable on the public Internet.
                if (segments[0] & 0xe000) != 0x2000 {
                    return false;
                }
                // 2002::/16 (6to4, RFC 3056) embeds an IPv4 address in octets 2..6.
                if segments[0] == 0x2002 {
                    let embedded_v4 = Ipv4Addr::new(octets[2], octets[3], octets[4], octets[5]);
                    return self.is_allowed_ipv4(embedded_v4);
                }
                // 2001:0000::/32 (Teredo, RFC 4380), 2001:0002::/48 (Benchmarking, RFC 5180),
                // 2001:0010::/28 & 2001:0020::/28 (ORCHID, RFC 4843 / RFC 7343),
                // 2001:0db8::/32 (Documentation, RFC 3849),
                // 3fff::/20 (Documentation, RFC 9637).
                if (segments[0] & 0xfff0) == 0x3ff0 {
                    return false;
                }
                if segments[0] == 0x2001
                    && (segments[1] == 0x0000
                        || (segments[1] == 0x0002 && segments[2] == 0)
                        || (segments[1] & 0xfff0) == 0x0010
                        || (segments[1] & 0xfff0) == 0x0020
                        || segments[1] == 0x0db8)
                {
                    return false;
                }
                true
            }
        }
    }
}

/// Relay address allocation strategy for TURN allocations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayAddressConfig {
    /// Bind ephemeral relay sockets on `bind_ip` and advertise the bound local address directly.
    None {
        /// Local IP address passed to `bind` when creating relay sockets.
        bind_ip: IpAddr,
    },
    /// Bind ephemeral relay sockets on `bind_ip` and advertise `relay_ip` in `XOR-RELAYED-ADDRESS`.
    Static {
        /// Public/external IP returned to TURN clients when an allocation is created.
        relay_ip: IpAddr,
        /// Local IP address passed to `bind` when creating relay sockets.
        bind_ip: IpAddr,
    },
    /// Bind relay sockets on `bind_ip` within `[min_port, max_port]` and advertise `relay_ip`.
    Range {
        /// Public/external IP returned to TURN clients when an allocation is created.
        relay_ip: IpAddr,
        /// Local IP address passed to `bind` when creating relay sockets.
        bind_ip: IpAddr,
        /// Inclusive lower relay UDP port bound.
        min_port: u16,
        /// Inclusive upper relay UDP port bound.
        max_port: u16,
        /// Maximum attempts to pick a free random port in `[min_port, max_port]`.
        max_retries: u16,
    },
}

impl Default for RelayAddressConfig {
    fn default() -> Self {
        Self::Static {
            relay_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        }
    }
}

impl RelayAddressConfig {
    /// Return the external/advertised IP address configured for relay allocations.
    pub const fn advertised_ip(&self) -> IpAddr {
        match *self {
            Self::None { bind_ip } => bind_ip,
            Self::Static { relay_ip, .. } | Self::Range { relay_ip, .. } => relay_ip,
        }
    }

    /// Validate address-family and port-range invariants before starting the TURN server.
    ///
    /// A configured range must satisfy `0 < min_port < max_port` so `[min_port, max_port]`
    /// contains at least one even and one odd port (required to service `EVEN-PORT` requests
    /// without stalling), and `relay_ip` must match the IP version of `bind_ip`.
    pub fn validate(&self) -> Result<()> {
        match *self {
            Self::None { .. } => Ok(()),
            Self::Static { relay_ip, bind_ip } => {
                if relay_ip.is_ipv4() != bind_ip.is_ipv4() {
                    return Err(TurnError::RelayAddressFamilyMismatch { relay_ip, bind_ip });
                }
                Ok(())
            }
            Self::Range {
                relay_ip,
                bind_ip,
                min_port,
                max_port,
                ..
            } => {
                if relay_ip.is_ipv4() != bind_ip.is_ipv4() {
                    return Err(TurnError::RelayAddressFamilyMismatch { relay_ip, bind_ip });
                }
                if min_port == 0 || max_port == 0 || min_port >= max_port {
                    return Err(TurnError::InvalidPortRange { min_port, max_port });
                }
                Ok(())
            }
        }
    }
}

/// Configuration for [`crate::TurnServer`].
#[derive(Clone)]
pub struct TurnServerConfig {
    /// UDP socket address the TURN server listens on.
    pub listen_addr: SocketAddr,
    /// Optional external/public IP advertised for the TURN listener in [`crate::TurnServer::advertised_addr`]
    /// and [`crate::TurnServer::ice_server_base_url`].
    ///
    /// When `None`, the listener's concrete bound IP is used if non-unspecified; if `listen_addr`
    /// is unspecified (`0.0.0.0` or `::`), the relay's advertised IP is used when it shares the
    /// listener's address family, falling back to loopback of the listener's family.
    pub advertised_ip: Option<IpAddr>,
    /// Realm advertised in TURN 401/438 challenges.
    pub realm: String,
    /// Secp256k1 server keypair used for Diffie-Hellman TURN credential derivation.
    pub server_key: SecretKey,
    /// Relay socket allocation strategy.
    pub relay: RelayAddressConfig,
    /// Destination IP policy enforced on relay sockets (`PublicOnly` by default).
    pub peer_policy: RelayPeerPolicy,
    /// Maximum number of active TURN nonce challenges retained concurrently.
    pub max_nonces: usize,
    /// Maximum number of active TURN relay allocations retained concurrently.
    pub max_allocations: usize,
    /// Maximum number of unclaimed [`crate::SignedTurnSessionTicket`] reservations retained
    /// concurrently (#1292). `0` disables ticket reservations.
    pub max_reservations: usize,
    /// Upper bound applied to unclaimed [`crate::SignedTurnSessionTicket`] reservation TTLs.
    pub max_reservation_ttl: Duration,
    /// Maximum number of active peer IP permissions retained concurrently per allocation.
    pub max_permissions_per_allocation: usize,
    /// ChannelBind lifetime (`Duration::ZERO` uses the RFC 5766 default of 10 minutes).
    pub channel_bind_timeout: Duration,
    /// Admission policy for verified delegator DIDs.
    pub admission: TurnAdmissionPolicy,
    /// Injectable clock used to validate [`Delegation`](rings_core::delegation::Delegation) windows.
    pub clock: SharedClock,
    /// Optional pluggable datagram relay factory (e.g. a multi-hop `onion::udp` session bridge).
    /// When `None`, [`crate::TurnServer`] binds local OS UDP relay sockets according to `relay`.
    pub custom_relay_factory: Option<Arc<dyn TurnDatagramRelayFactory>>,
}

impl std::fmt::Debug for TurnServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnServerConfig")
            .field("listen_addr", &self.listen_addr)
            .field("advertised_ip", &self.advertised_ip)
            .field("realm", &self.realm)
            .field("server_pubkey", &self.server_key.pubkey())
            .field("relay", &self.relay)
            .field("peer_policy", &self.peer_policy)
            .field("max_nonces", &self.max_nonces)
            .field("max_allocations", &self.max_allocations)
            .field("max_reservations", &self.max_reservations)
            .field("max_reservation_ttl", &self.max_reservation_ttl)
            .field(
                "max_permissions_per_allocation",
                &self.max_permissions_per_allocation,
            )
            .field("channel_bind_timeout", &self.channel_bind_timeout)
            .field("admission", &self.admission)
            .field(
                "has_custom_relay_factory",
                &self.custom_relay_factory.is_some(),
            )
            .finish()
    }
}

impl Default for TurnServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_TURN_PORT),
            advertised_ip: None,
            realm: DEFAULT_TURN_REALM.to_string(),
            server_key: SecretKey::random(),
            relay: RelayAddressConfig::default(),
            peer_policy: RelayPeerPolicy::PublicOnly,
            max_nonces: DEFAULT_MAX_NONCES,
            max_allocations: DEFAULT_MAX_ALLOCATIONS,
            max_reservations: DEFAULT_MAX_RESERVATIONS,
            max_reservation_ttl: DEFAULT_MAX_RESERVATION_TTL,
            max_permissions_per_allocation: DEFAULT_MAX_PERMISSIONS_PER_ALLOCATION,
            channel_bind_timeout: Duration::ZERO,
            admission: TurnAdmissionPolicy::default(),
            clock: Arc::new(SystemClock),
            custom_relay_factory: None,
        }
    }
}

impl TurnServerConfig {
    /// Create a loopback TURN server configuration bound to an ephemeral port (`127.0.0.1:0`),
    /// suitable for local testing and in-process integration tests.
    pub fn loopback_ephemeral() -> Self {
        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        Self {
            listen_addr: SocketAddr::new(localhost, 0),
            advertised_ip: None,
            realm: DEFAULT_TURN_REALM.to_string(),
            server_key: SecretKey::random(),
            relay: RelayAddressConfig::Static {
                relay_ip: localhost,
                bind_ip: localhost,
            },
            peer_policy: RelayPeerPolicy::AllowLocalAndPrivate,
            max_nonces: DEFAULT_MAX_NONCES,
            max_allocations: DEFAULT_MAX_ALLOCATIONS,
            max_reservations: DEFAULT_MAX_RESERVATIONS,
            max_reservation_ttl: DEFAULT_MAX_RESERVATION_TTL,
            max_permissions_per_allocation: DEFAULT_MAX_PERMISSIONS_PER_ALLOCATION,
            channel_bind_timeout: Duration::ZERO,
            admission: TurnAdmissionPolicy::AllowAnyDelegation,
            clock: Arc::new(SystemClock),
            custom_relay_factory: None,
        }
    }
}
