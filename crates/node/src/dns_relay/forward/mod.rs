//! The relay's forward: an accepted connection carried to the owner's node over the TCP relay.
//!
//! ```text
//!   forward(s, o, p, h) = carry h·s over the TCP relay to o's relay_service(p)   o ≠ this node
//!                       = splice h·s with this node's own target of p           o = this node
//! ```
//!
//! The owner's node dials only the services it registered, and it registers only
//! [`relay_service(p)`](super::relay_service) for each port `p` it serves, so a connection the
//! ingress carries reaches exactly the local target the owner mapped to that port. A relay node
//! that serves ports itself is its own owner: the overlay has no hop to itself, so its own
//! connections go straight to its own targets. Either way the target reads the preamble `h` (a
//! PROXY protocol header, or nothing) before the client's bytes.
//!
//! Either way the forward returns only when the connection is done, so the ingress holds the
//! connection's admission slot for its whole life; a connection that carries nothing for
//! [`RELAY_IDLE_TIMEOUT`] is done, as a TCP relay session is.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use rings_dns_relay::Connection;
use rings_dns_relay::Forward;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio::time::Instant;

use super::relay_service;
use super::DidOwner;
use crate::extension::protocols::relay::RelayHandle;
use crate::extension::transport::RELAY_IDLE_TIMEOUT;
use crate::prelude::rings_core::dht::Did;

/// Bytes one read of a spliced connection takes at most.
const SPLICE_BUF: usize = 16 * 1024;

/// Why a connection could not be carried, or ended abnormally.
#[derive(Debug, Error)]
pub enum ForwardError {
    /// The connection is for this node, which serves no such port.
    #[error("this node serves no port {0}")]
    NotServed(NonZeroU16),
    /// This node's own target cannot be reached.
    #[error("cannot reach {target}: {source}")]
    Connect {
        /// The target.
        target: SocketAddr,
        /// Why.
        source: std::io::Error,
    },
    /// A spliced connection failed.
    #[error("spliced connection failed: {0}")]
    Splice(#[source] std::io::Error),
    /// A spliced connection carried nothing for this long, so it was closed.
    #[error("spliced connection idle for {0:?}")]
    Idle(Duration),
    /// The TCP relay could not carry the connection.
    #[error("TCP relay: {0}")]
    Relay(#[source] crate::error::Error),
}

/// The forward over a node's TCP relay.
pub struct RelayForward {
    relay: RelayHandle,
    own: Did,
    own_targets: BTreeMap<u16, SocketAddr>,
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
            own_targets: own_targets.into_iter().collect(),
        }
    }

    /// Splice `connection` with this node's own target of its port.
    async fn splice_own(&self, connection: Connection<DidOwner>) -> Result<(), ForwardError> {
        let Connection {
            stream,
            port,
            preamble,
            ..
        } = connection;
        let target = *self
            .own_targets
            .get(&port.get())
            .ok_or(ForwardError::NotServed(port))?;
        let mut upstream = TcpStream::connect(target)
            .await
            .map_err(|source| ForwardError::Connect { target, source })?;
        within(RELAY_IDLE_TIMEOUT, upstream.write_all(&preamble)).await?;
        splice(stream, upstream, RELAY_IDLE_TIMEOUT).await
    }
}

#[async_trait]
impl Forward for RelayForward {
    type Error = ForwardError;
    type Owner = DidOwner;

    async fn forward(&self, connection: Connection<DidOwner>) -> Result<(), ForwardError> {
        if connection.owner.0 == self.own {
            return self.splice_own(connection).await;
        }
        let Connection {
            stream,
            owner,
            port,
            preamble,
        } = connection;
        self.relay
            .carry_tcp_stream(
                stream,
                owner.0,
                relay_service(port.get()),
                Bytes::from(preamble),
            )
            .await
            .map_err(ForwardError::Relay)
    }
}

/// `io`, unless it takes longer than `idle`.
async fn within<T>(
    idle: Duration,
    io: impl std::future::Future<Output = std::io::Result<T>>,
) -> Result<T, ForwardError> {
    timeout(idle, io)
        .await
        .map_err(|_elapsed| ForwardError::Idle(idle))?
        .map_err(ForwardError::Splice)
}

/// Splice `client` with `upstream` until both directions have ended, each passing on the other's
/// end of stream, or until no byte has moved for `idle`.
///
/// Only reads are raced: a read is cancel-safe, a write is not, so a write runs to its end (or
/// to `idle`) before the next read.
pub(crate) async fn splice(
    client: TcpStream,
    upstream: TcpStream,
    idle: Duration,
) -> Result<(), ForwardError> {
    let (mut client_read, mut client_write) = client.into_split();
    let (mut upstream_read, mut upstream_write) = upstream.into_split();
    let mut up = vec![0u8; SPLICE_BUF];
    let mut down = vec![0u8; SPLICE_BUF];
    let (mut up_open, mut down_open) = (true, true);
    let deadline = tokio::time::sleep(idle);
    tokio::pin!(deadline);
    while up_open || down_open {
        tokio::select! {
            read = client_read.read(&mut up), if up_open => {
                up_open = pass_on(read, &up, &mut upstream_write, idle).await?;
            }
            read = upstream_read.read(&mut down), if down_open => {
                down_open = pass_on(read, &down, &mut client_write, idle).await?;
            }
            () = &mut deadline => return Err(ForwardError::Idle(idle)),
        }
        deadline.as_mut().reset(Instant::now() + idle);
    }
    Ok(())
}

/// Pass on what one read into `buf` gave: write the bytes to `write`, or at the end of the
/// stream shut `write` down. Return whether the direction is still open.
async fn pass_on(
    read: std::io::Result<usize>,
    buf: &[u8],
    write: &mut OwnedWriteHalf,
    idle: Duration,
) -> Result<bool, ForwardError> {
    let len = read.map_err(ForwardError::Splice)?;
    if len == 0 {
        within(idle, write.shutdown()).await?;
        return Ok(false);
    }
    let bytes = buf
        .get(..len)
        .ok_or_else(|| ForwardError::Splice(std::io::Error::other("read past its buffer")))?;
    within(idle, write.write_all(bytes)).await?;
    Ok(true)
}

#[cfg(test)]
mod test_forward;
