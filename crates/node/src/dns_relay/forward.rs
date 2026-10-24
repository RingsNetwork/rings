//! The relay's forward: an accepted connection carried to the owner's node over the TCP relay.
//!
//! ```text
//!   forward(s, o, p) = carry s over the TCP relay to o's relay_service(p)    o ≠ this node
//!                    = splice s with this node's own target of p             o = this node
//! ```
//!
//! The owner's node dials only the services it registered, and it registers only
//! [`relay_service(p)`](super::relay_service) for each port `p` it serves, so a connection the
//! ingress carries reaches exactly the local target the owner mapped to that port. A relay node
//! that serves ports itself is its own owner: the overlay has no hop to itself, so its own
//! connections go straight to its own targets.
//!
//! Either way the forward returns only when the connection is done, so the ingress holds the
//! connection's admission slot for its whole life.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use async_trait::async_trait;
use rings_dns_relay::Forward;
use rings_dns_relay::ForwardFailure;
use rings_dns_relay::ServiceName;
use tokio::net::TcpStream;

use super::relay_service;
use super::DidOwner;
use crate::extension::protocols::relay::RelayHandle;
use crate::prelude::rings_core::dht::Did;

/// The forward over a node's TCP relay.
pub struct RelayForward {
    relay: RelayHandle,
    own: Did,
    own_targets: BTreeMap<String, SocketAddr>,
}

impl RelayForward {
    /// The forward over `relay`, run by the node `own`, which serves each of `own_targets`'
    /// ports at its local address.
    pub fn new(
        relay: RelayHandle,
        own: Did,
        own_targets: impl IntoIterator<Item = (u16, SocketAddr)>,
    ) -> Self {
        Self {
            relay,
            own,
            own_targets: own_targets
                .into_iter()
                .map(|(port, target)| (relay_service(port), target))
                .collect(),
        }
    }

    /// Splice `stream` with this node's own target of `service`.
    async fn splice_own(
        &self,
        mut stream: TcpStream,
        service: &ServiceName,
    ) -> Result<(), ForwardFailure> {
        let target = self
            .own_targets
            .get(service.as_str())
            .ok_or_else(|| ForwardFailure(format!("this node serves no {service}")))?;
        let mut upstream = TcpStream::connect(target)
            .await
            .map_err(|error| ForwardFailure(format!("{target}: {error}")))?;
        tokio::io::copy_bidirectional(&mut stream, &mut upstream)
            .await
            .map(|_| ())
            .map_err(|error| ForwardFailure(error.to_string()))
    }
}

#[async_trait]
impl Forward for RelayForward {
    type Owner = DidOwner;

    async fn forward(
        &self,
        stream: TcpStream,
        owner: DidOwner,
        service: &ServiceName,
    ) -> Result<(), ForwardFailure> {
        if owner.0 == self.own {
            return self.splice_own(stream, service).await;
        }
        self.relay
            .carry_tcp_stream(stream, owner.0, service.as_str().to_owned())
            .await
            .map_err(|error| ForwardFailure(error.to_string()))
    }
}
