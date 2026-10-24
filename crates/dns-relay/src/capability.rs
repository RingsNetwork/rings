//! The two capabilities a relay needs from the node that embeds it.
//!
//! The relay knows names, addresses and connections; the node knows the overlay. The node hands
//! the relay exactly two capabilities, and nothing else of itself:
//!
//! - [`Directory`]: read what an owner announces (in Rings, its signed descriptor in the DHT);
//! - [`Forward`]: carry an accepted connection to one of an owner's ports (in Rings, over the
//!   TCP relay to the owner's node, which dials only the ports it serves).
//!
//! Each names its own error type, so a failure keeps its cause; the relay only logs it and
//! answers the way the failure's kind demands (`SERVFAIL`, a closed connection).

use std::num::NonZeroU16;

use async_trait::async_trait;
use tokio::net::TcpStream;

use crate::announcement::Announcement;
use crate::owner::Owner;

/// Reads what owners announce.
#[async_trait]
pub trait Directory: Send + Sync + 'static {
    /// The owners this directory knows.
    type Owner: Owner;
    /// Why a lookup failed; the answer is then unknown, not negative.
    type Error: std::error::Error + Send + Sync + 'static;

    /// What `owner` announces now: `Ok(None)` when it announces nothing valid, and `Err` when
    /// the directory cannot tell.
    async fn announcement(&self, owner: Self::Owner) -> Result<Option<Announcement>, Self::Error>;
}

/// One accepted connection for one of an owner's ports.
#[derive(Debug)]
pub struct Connection<O> {
    /// The client's stream.
    pub stream: TcpStream,
    /// The owner whose address it reached.
    pub owner: O,
    /// The port it reached.
    pub port: NonZeroU16,
    /// Bytes the service must read before the client's: a PROXY protocol header, or nothing.
    pub preamble: Vec<u8>,
}

/// Carries accepted connections to owners' ports.
#[async_trait]
pub trait Forward: Send + Sync + 'static {
    /// The owners this capability reaches.
    type Owner: Owner;
    /// Why a connection could not be carried.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Carry `connection` to its owner's port, its preamble first, and return only when the
    /// connection is done, so its admission slot is held for its whole life.
    async fn forward(&self, connection: Connection<Self::Owner>) -> Result<(), Self::Error>;
}
