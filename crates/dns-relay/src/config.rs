//! The relay's configuration: its zones, prefix, ports, listeners and state directory.
//!
//! This is the serialized form an operator writes (the node embeds it in `config.yaml`). It is
//! validated once, by [`DnsRelay::open`](crate::DnsRelay::open), into the typed values the relay
//! runs on; unknown keys are rejected.

use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::path::PathBuf;

use ipnet::Ipv6Net;
use serde::Deserialize;
use serde::Serialize;

use crate::ingress::DEFAULT_MAX_CONNECTIONS;
use crate::ingress::DEFAULT_MAX_CONNECTIONS_PER_OWNER;

/// The port DNS is served on.
const DNS_PORT: u16 = 53;
/// Default TTL of positive answers, in seconds.
const DEFAULT_TTL_SECS: u32 = 60;
/// Default TTL of negative answers, in seconds.
const DEFAULT_NEGATIVE_TTL_SECS: u32 = 60;
/// Default time a binding stays in the address book unanswered, in seconds: eight days, beyond
/// the grace period, so a remembered owner keeps its address too.
const DEFAULT_BOOK_RETENTION_SECS: u64 = 8 * 86_400;
/// Default grace period of a vanished owner, in seconds: seven days, beyond the five days a
/// mail server commonly retries a delivery before it bounces it.
const DEFAULT_GRACE_SECS: u64 = 7 * 86_400;
/// Default freshness window of an owner's last lookup, in seconds.
const DEFAULT_FRESH_SECS: u64 = 15;
/// Default bound on the owners the relay keeps an address and a memory for.
const DEFAULT_MAX_OWNERS: usize = 65_536;
/// Default bound on the directory lookups in flight.
const DEFAULT_MAX_LOOKUPS: usize = 64;

/// One zone delegated to the relay.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ZoneConfig {
    /// The zone's origin, such as `rings.rs` or `p2p.example.com`.
    pub origin: String,
    /// The zone's nameservers, as the parent's NS records name them, the primary (which the SOA
    /// names) first. There is at least one, and they lie outside every relayed zone.
    pub nameservers: Vec<String>,
    /// The mailbox the zone's SOA names, in DNS form: `hostmaster.example.com` for
    /// `hostmaster@example.com`.
    pub contact: String,
}

/// The relay's configuration.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DnsRelayConfig {
    /// The zones delegated to this relay.
    pub zones: Vec<ZoneConfig>,
    /// The IPv6 prefix routed to this host, in which every owner gets its own address.
    pub prefix: Ipv6Net,
    /// The ports the ingress accepts on, for every address in the prefix.
    pub ports: Vec<u16>,
    /// The directory holding the address key, the address book and the memory of
    /// announcements.
    pub state_dir: PathBuf,
    /// The addresses DNS is served on, over UDP and TCP.
    #[serde(default = "default_dns_listen")]
    pub dns_listen: Vec<SocketAddr>,
    /// The IPv6 address the ingress listens on; the wildcard receives the whole prefix.
    #[serde(default = "default_ingress_listen")]
    pub ingress_listen: Ipv6Addr,
    /// TTL of positive answers, in seconds.
    #[serde(default = "default_ttl_secs")]
    pub ttl_secs: u32,
    /// TTL of negative answers, in seconds.
    #[serde(default = "default_negative_ttl_secs")]
    pub negative_ttl_secs: u32,
    /// How long a binding stays in the address book unanswered, in seconds.
    #[serde(default = "default_book_retention_secs")]
    pub book_retention_secs: u64,
    /// How long the relay keeps answering for an owner whose announcement vanished from the
    /// directory, in seconds.
    #[serde(default = "default_grace_secs")]
    pub grace_secs: u64,
    /// How long an owner's last lookup answers without asking the directory again, in seconds.
    #[serde(default = "default_fresh_secs")]
    pub fresh_secs: u64,
    /// Most owners the relay keeps an address binding and a memory for; at least 1.
    #[serde(default = "default_max_owners")]
    pub max_owners: usize,
    /// Most directory lookups in flight; at least 1. A query that finds them all taken is
    /// answered from memory, or with `SERVFAIL`.
    #[serde(default = "default_max_lookups")]
    pub max_lookups: usize,
    /// Most connections carried at once.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// Most connections carried at once to one owner.
    #[serde(default = "default_max_connections_per_owner")]
    pub max_connections_per_owner: usize,
}

/// DNS on port 53 of every IPv6 and IPv4 address.
fn default_dns_listen() -> Vec<SocketAddr> {
    vec![
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), DNS_PORT),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DNS_PORT),
    ]
}

/// The IPv6 wildcard.
const fn default_ingress_listen() -> Ipv6Addr {
    Ipv6Addr::UNSPECIFIED
}

/// [`DEFAULT_TTL_SECS`].
const fn default_ttl_secs() -> u32 {
    DEFAULT_TTL_SECS
}

/// [`DEFAULT_NEGATIVE_TTL_SECS`].
const fn default_negative_ttl_secs() -> u32 {
    DEFAULT_NEGATIVE_TTL_SECS
}

/// [`DEFAULT_BOOK_RETENTION_SECS`].
const fn default_book_retention_secs() -> u64 {
    DEFAULT_BOOK_RETENTION_SECS
}

/// [`DEFAULT_GRACE_SECS`].
const fn default_grace_secs() -> u64 {
    DEFAULT_GRACE_SECS
}

/// [`DEFAULT_FRESH_SECS`].
const fn default_fresh_secs() -> u64 {
    DEFAULT_FRESH_SECS
}

/// [`DEFAULT_MAX_OWNERS`].
const fn default_max_owners() -> usize {
    DEFAULT_MAX_OWNERS
}

/// [`DEFAULT_MAX_LOOKUPS`].
const fn default_max_lookups() -> usize {
    DEFAULT_MAX_LOOKUPS
}

/// [`DEFAULT_MAX_CONNECTIONS`].
const fn default_max_connections() -> usize {
    DEFAULT_MAX_CONNECTIONS
}

/// [`DEFAULT_MAX_CONNECTIONS_PER_OWNER`].
const fn default_max_connections_per_owner() -> usize {
    DEFAULT_MAX_CONNECTIONS_PER_OWNER
}
