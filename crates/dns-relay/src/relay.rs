//! The relay: its DNS authority and its ingress, opened from one configuration and run together.
//!
//! ```text
//!   open : Config → (Directory, Forward) → Result DnsRelay      validates, reads the state
//!   run  : DnsRelay → stop → Result ()                           binds, serves until stop
//! ```
//!
//! `open` changes nothing outside the state directory, and `run` binds every socket before it
//! serves any, so a misconfigured relay fails before it answers anything.

use std::collections::BTreeSet;
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use hickory_proto::rr::Name;
use hickory_proto::ProtoError;
use hickory_server::net::NetError;
use hickory_server::Server;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::net::UdpSocket;

use crate::authority::AnswerTtl;
use crate::capability::Directory;
use crate::capability::Forward;
use crate::config::DnsRelayConfig;
use crate::config::ZoneConfig;
use crate::ingress::AdmissionLimits;
use crate::ingress::Ingress;
use crate::ingress::IngressError;
use crate::memory::MemoryWindows;
use crate::recall::Recall;
use crate::registry::duration_ms;
use crate::registry::AddressRegistry;
use crate::server::DnsAuthority;
use crate::state::StateError;
use crate::zone::Zone;
use crate::zone::ZoneError;
use crate::zone::Zones;

/// How long a TCP DNS client may stay silent before the relay closes its connection.
const DNS_TCP_TIMEOUT: Duration = Duration::from_secs(5);
/// How many responses may queue on one TCP DNS connection.
const DNS_TCP_RESPONSE_BUFFER: usize = 32;

/// Why a relay cannot open or run.
#[derive(Debug, Error)]
pub enum DnsRelayError {
    /// A domain name in the configuration does not parse.
    #[error("invalid domain name {name:?}: {source}")]
    Name {
        /// The name as written.
        name: String,
        /// Why it does not parse.
        source: ProtoError,
    },
    /// A zone names no nameserver.
    #[error("zone {0} names no nameserver")]
    NoNameserver(String),
    /// The zones cannot be served together.
    #[error(transparent)]
    Zone(#[from] ZoneError),
    /// No ingress port is configured, or port 0 is.
    #[error("the ingress needs at least one port, and port 0 cannot be served")]
    Ports,
    /// No DNS listener is configured.
    #[error("DNS needs at least one listening address")]
    DnsListen,
    /// A bound that must admit something is 0.
    #[error("{0} must be at least 1")]
    ZeroBound(&'static str),
    /// The state directory cannot be read or written.
    #[error(transparent)]
    State(#[from] StateError),
    /// A DNS socket cannot be bound.
    #[error("DNS cannot listen on {address}: {source}")]
    DnsBind {
        /// The address.
        address: SocketAddr,
        /// The failure.
        source: std::io::Error,
    },
    /// The ingress cannot be bound.
    #[error(transparent)]
    Ingress(#[from] IngressError),
    /// The DNS server failed while shutting down.
    #[error("DNS server: {0}")]
    Dns(#[from] NetError),
}

/// A relay opened from its configuration, ready to run.
pub struct DnsRelay<D, F>
where
    D: Directory,
    F: Forward<Owner = D::Owner>,
{
    authority: DnsAuthority<Recall<D>>,
    dns_listen: Vec<SocketAddr>,
    ingress: Ingress<Recall<D>, F>,
}

impl<D, F> DnsRelay<D, F>
where
    D: Directory,
    F: Forward<Owner = D::Owner>,
{
    /// Validate `config` and open the relay's state, to read owners from `directory`, through
    /// the relay's memory of them, and carry connections with `forward`.
    pub async fn open(
        config: &DnsRelayConfig,
        directory: Arc<D>,
        forward: Arc<F>,
    ) -> Result<Self, DnsRelayError> {
        let zones = Arc::new(Zones::new(
            config
                .zones
                .iter()
                .map(zone_of)
                .collect::<Result<Vec<_>, _>>()?,
        )?);
        let ports = ingress_ports(&config.ports)?;
        if config.dns_listen.is_empty() {
            return Err(DnsRelayError::DnsListen);
        }
        let max_owners = at_least_one(config.max_owners, "max_owners")?;
        let max_lookups = at_least_one(config.max_lookups, "max_lookups")?;
        let registry = Arc::new(
            AddressRegistry::open(
                config.prefix,
                &config.state_dir,
                Duration::from_secs(config.book_retention_secs),
                max_owners,
            )
            .await?,
        );
        let windows = MemoryWindows {
            fresh_ms: duration_ms(Duration::from_secs(config.fresh_secs)),
            grace_ms: duration_ms(Duration::from_secs(config.grace_secs)),
        };
        let recall = Arc::new(
            Recall::open(
                directory,
                &config.state_dir,
                windows,
                max_owners,
                max_lookups,
            )
            .await?,
        );
        let ttl = AnswerTtl {
            positive: config.ttl_secs,
            negative: config.negative_ttl_secs,
        };
        let limits = AdmissionLimits {
            total: config.max_connections,
            per_owner: config.max_connections_per_owner,
        };
        Ok(Self {
            authority: DnsAuthority::new(zones, ttl, Arc::clone(&registry), Arc::clone(&recall)),
            dns_listen: config.dns_listen.clone(),
            ingress: Ingress::new(
                config.ingress_listen,
                ports,
                registry,
                recall,
                forward,
                limits,
            ),
        })
    }

    /// Bind every socket, then serve DNS and the ingress until `stop` completes.
    pub async fn run(self, stop: impl Future<Output = ()>) -> Result<(), DnsRelayError> {
        let mut server = Server::new(self.authority);
        for address in &self.dns_listen {
            let bind_error = |source| DnsRelayError::DnsBind {
                address: *address,
                source,
            };
            let udp = UdpSocket::bind(address).await.map_err(bind_error)?;
            let tcp = TcpListener::bind(address).await.map_err(bind_error)?;
            server.register_socket(udp);
            server.register_listener(tcp, DNS_TCP_TIMEOUT, DNS_TCP_RESPONSE_BUFFER);
        }
        let listeners = self.ingress.bind()?;
        let mut ingress = self.ingress.serve(listeners);
        stop.await;
        ingress.abort_all();
        server.shutdown_gracefully().await?;
        Ok(())
    }
}

/// The ingress ports `ports` names: at least one, none of them 0.
fn ingress_ports(ports: &[u16]) -> Result<BTreeSet<NonZeroU16>, DnsRelayError> {
    let ports = ports
        .iter()
        .map(|port| NonZeroU16::new(*port).ok_or(DnsRelayError::Ports))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if ports.is_empty() {
        return Err(DnsRelayError::Ports);
    }
    Ok(ports)
}

/// `value`, the bound named `name`, when it admits something.
fn at_least_one(value: usize, name: &'static str) -> Result<usize, DnsRelayError> {
    if value == 0 {
        return Err(DnsRelayError::ZeroBound(name));
    }
    Ok(value)
}

/// The zone `config` describes: its first nameserver is the primary.
fn zone_of(config: &ZoneConfig) -> Result<Zone, DnsRelayError> {
    let mut nameservers = config
        .nameservers
        .iter()
        .map(|nameserver| fqdn(nameserver))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter();
    let primary = nameservers
        .next()
        .ok_or_else(|| DnsRelayError::NoNameserver(config.origin.clone()))?;
    Ok(Zone::new(
        fqdn(&config.origin)?,
        primary,
        nameservers.collect(),
        fqdn(&config.contact)?,
    ))
}

/// `name` as a fully qualified domain name: a written name is absolute whether or not it ends
/// with a dot.
fn fqdn(name: &str) -> Result<Name, DnsRelayError> {
    let mut parsed = Name::from_ascii(name).map_err(|source| DnsRelayError::Name {
        name: name.to_owned(),
        source,
    })?;
    parsed.set_fqdn(true);
    Ok(parsed)
}
