//! What `rings run` starts of the DNS relay: the ports the node serves through relays,
//! registered with its TCP relay, and the relay its configuration asks it to run.

use std::net::SocketAddr;
use std::sync::Arc;

use rings_dns_relay::DnsRelay;
use rings_dns_relay::DnsRelayConfig;
use rings_dns_relay::DnsRelayError;
use thiserror::Error;

use super::relay_service;
use super::DhtDirectory;
use super::RelayForward;
use super::ServicesConfig;
use crate::extension::protocols::relay::RelayHandle;
use crate::processor::Processor;
use crate::util::expand_home;

/// The relay a node runs: over its DHT and its TCP relay.
pub type NodeDnsRelay = DnsRelay<DhtDirectory, RelayForward>;

/// Why the node cannot serve its ports or open its relay.
#[derive(Debug, Error)]
pub enum OpenError {
    /// A served port's target cannot be registered with the TCP relay.
    #[error("cannot serve port {port} at {target}: {source}")]
    Serve {
        /// The port.
        port: u16,
        /// Its local target.
        target: SocketAddr,
        /// Why.
        source: crate::error::Error,
    },
    /// The relay's state directory cannot be resolved.
    #[error("DNS relay state directory: {0}")]
    StateDir(#[source] crate::error::Error),
    /// The relay cannot open.
    #[error("cannot open the DNS relay: {0}")]
    Relay(#[from] DnsRelayError),
}

/// Register the target of every port in `services` with `relay`, as the TCP relay service the
/// relays' ingress asks for; then open the relay `config` asks for, over `processor`'s DHT and
/// `relay`, with this node's own served ports dialed directly. `None` when it asks for none.
pub async fn open(
    services: Option<&ServicesConfig>,
    config: Option<&DnsRelayConfig>,
    processor: &Arc<Processor>,
    relay: &RelayHandle,
) -> Result<Option<NodeDnsRelay>, OpenError> {
    let targets = || services.into_iter().flat_map(ServicesConfig::targets);
    for (port, target) in targets() {
        relay
            .register_tcp_service(relay_service(port), target)
            .await
            .map_err(|source| OpenError::Serve {
                port,
                target,
                source,
            })?;
    }
    let Some(config) = config else {
        return Ok(None);
    };
    let config = DnsRelayConfig {
        state_dir: expand_home(&config.state_dir).map_err(OpenError::StateDir)?,
        ..config.clone()
    };
    let directory = Arc::new(DhtDirectory::new(Arc::clone(processor)));
    let forward = Arc::new(RelayForward::new(relay.clone(), processor.did(), targets()));
    Ok(Some(DnsRelay::open(&config, directory, forward).await?))
}
