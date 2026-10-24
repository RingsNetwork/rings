//! The PROXY protocol header the ingress writes before a connection's bytes, for a port whose
//! owner asked for it ([`ProxyProtocol::V2`](crate::ProxyProtocol::V2)).
//!
//! The relay carries bytes only, so the service behind a relayed port sees every connection come
//! from the owner's node. With the header, it learns the client's address and the address the
//! client reached, as HAProxy's PROXY protocol version 2 states them (`ppp` builds the header).
//!
//! The header is an assertion by whoever opened the stream: an owner's served ports are TCP relay
//! services every overlay peer can open, so a service must treat the header as the relay's word,
//! not as proof of origin.

use std::net::SocketAddr;

use ppp::v2::Builder;
use ppp::v2::Command;
use ppp::v2::Protocol;
use ppp::v2::Version;
use thiserror::Error;

/// The header could not be built.
#[derive(Debug, Error)]
#[error("PROXY protocol header: {0}")]
pub struct ProxyHeaderError(#[from] std::io::Error);

/// The PROXY protocol version 2 header of a TCP connection from `client` to `relayed`.
///
/// Law: `ppp::v2::Header::try_from(h)` parses `h` back to `(client, relayed)`.
pub fn proxy_v2_header(
    client: SocketAddr,
    relayed: SocketAddr,
) -> Result<Vec<u8>, ProxyHeaderError> {
    Builder::with_addresses(
        Version::Two | Command::Proxy,
        Protocol::Stream,
        (client, relayed),
    )
    .build()
    .map_err(ProxyHeaderError)
}

#[cfg(test)]
mod test_proxy;
