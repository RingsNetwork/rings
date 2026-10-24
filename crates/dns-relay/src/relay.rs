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
use std::sync::Arc;
use std::time::Duration;

use hickory_proto::rr::Name;
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
use crate::registry::AddressRegistry;
use crate::remember::Remembering;
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
    #[error("invalid domain name {name:?}: {reason}")]
    Name {
        /// The name as written.
        name: String,
        /// Why it does not parse.
        reason: String,
    },
    /// The zones cannot be served together.
    #[error(transparent)]
    Zone(#[from] ZoneError),
    /// No ingress port is configured, or port 0 is.
    #[error("the ingress needs at least one port, and port 0 cannot be served")]
    Ports,
    /// No DNS listener is configured.
    #[error("DNS needs at least one listening address")]
    DnsListen,
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
    Dns(String),
}

/// A relay opened from its configuration, ready to run.
pub struct DnsRelay<D, F>
where
    D: Directory,
    F: Forward<Owner = D::Owner>,
{
    authority: DnsAuthority<Remembering<D>>,
    dns_listen: Vec<SocketAddr>,
    ingress: Ingress<Remembering<D>, F>,
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
        let ports = config.ports.iter().copied().collect::<BTreeSet<_>>();
        if ports.is_empty() || ports.contains(&0) {
            return Err(DnsRelayError::Ports);
        }
        if config.dns_listen.is_empty() {
            return Err(DnsRelayError::DnsListen);
        }
        let registry = Arc::new(
            AddressRegistry::open(
                config.prefix,
                &config.state_dir,
                Duration::from_secs(config.book_retention_secs),
            )
            .await?,
        );
        let directory = Arc::new(
            Remembering::open(
                directory,
                &config.state_dir,
                Duration::from_secs(config.grace_secs),
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
            authority: DnsAuthority::new(zones, ttl, Arc::clone(&registry), Arc::clone(&directory)),
            dns_listen: config.dns_listen.clone(),
            ingress: Ingress::new(
                config.ingress_listen,
                ports,
                registry,
                directory,
                forward,
                limits,
            ),
        })
    }

    /// Bind every socket, then serve DNS and the ingress until `stop` completes.
    pub async fn run(self, stop: impl Future<Output = ()>) -> Result<(), DnsRelayError> {
        let mut server = Server::new(self.authority);
        for address in &self.dns_listen {
            let udp = UdpSocket::bind(address)
                .await
                .map_err(|source| DnsRelayError::DnsBind {
                    address: *address,
                    source,
                })?;
            let tcp =
                TcpListener::bind(address)
                    .await
                    .map_err(|source| DnsRelayError::DnsBind {
                        address: *address,
                        source,
                    })?;
            server.register_socket(udp);
            server.register_listener(tcp, DNS_TCP_TIMEOUT, DNS_TCP_RESPONSE_BUFFER);
        }
        let listeners = self.ingress.bind().await?;
        let mut ingress = self.ingress.serve(listeners);
        stop.await;
        ingress.abort_all();
        server
            .shutdown_gracefully()
            .await
            .map_err(|error| DnsRelayError::Dns(error.to_string()))
    }
}

/// The zone `config` describes.
fn zone_of(config: &ZoneConfig) -> Result<Zone, DnsRelayError> {
    Ok(Zone::new(
        fqdn(&config.origin)?,
        config
            .nameservers
            .iter()
            .map(|nameserver| fqdn(nameserver))
            .collect::<Result<Vec<_>, _>>()?,
        fqdn(&config.contact)?,
    ))
}

/// `name` as a fully qualified domain name: a written name is absolute whether or not it ends
/// with a dot.
fn fqdn(name: &str) -> Result<Name, DnsRelayError> {
    let mut parsed = Name::from_ascii(name).map_err(|error| DnsRelayError::Name {
        name: name.to_owned(),
        reason: error.to_string(),
    })?;
    parsed.set_fqdn(true);
    Ok(parsed)
}
