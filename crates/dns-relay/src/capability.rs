//! The two capabilities a relay needs from the node that embeds it.
//!
//! The relay knows names, addresses and connections; the node knows the overlay. The node hands
//! the relay exactly two capabilities, and nothing else of itself:
//!
//! - [`Directory`]: read what an owner announces (in Rings, its signed descriptor in the DHT);
//! - [`Forward`]: carry an accepted connection to one of an owner's services (in Rings, over the
//!   TCP relay to the owner's node, which dials only services it registered).

use async_trait::async_trait;
use thiserror::Error;
use tokio::net::TcpStream;

use crate::announcement::Announcement;
use crate::announcement::ServiceName;
use crate::owner::Owner;

/// The directory could not be read: the answer is unknown, not negative.
///
/// The DNS authority answers `SERVFAIL`, which a resolver retries and does not cache as a
/// non-existent name, and the ingress closes the connection.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("announcement lookup failed: {0}")]
pub struct LookupFailure(pub String);

/// A connection could not be carried to the owner's service.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("forwarding failed: {0}")]
pub struct ForwardFailure(pub String);

/// Reads what owners announce.
#[async_trait]
pub trait Directory: Send + Sync + 'static {
    /// The owners this directory knows.
    type Owner: Owner;

    /// What `owner` announces now: `Ok(None)` when it announces nothing valid, and `Err` when
    /// the directory cannot tell.
    async fn announcement(&self, owner: Self::Owner)
        -> Result<Option<Announcement>, LookupFailure>;
}

/// Carries accepted connections to owners' services.
#[async_trait]
pub trait Forward: Send + Sync + 'static {
    /// The owners this capability reaches.
    type Owner: Owner;

    /// Carry `stream` to `owner`'s service `service`, until either end closes it.
    async fn forward(
        &self,
        stream: TcpStream,
        owner: Self::Owner,
        service: &ServiceName,
    ) -> Result<(), ForwardFailure>;
}
