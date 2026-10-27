//! The relay's listening sockets, bound alike whatever the system's defaults: a socket bound to
//! an IPv6 address receives IPv6 only, so the IPv6 and IPv4 wildcards of one port are two
//! disjoint sockets.
//!
//! ```text
//!   bind_udp   : SocketAddr → io::Result UdpSocket
//!   listen_tcp : SocketAddr → io::Result TcpListener
//!
//!   a ∈ IPv6  ⇒  the socket bound to a holds port(a) of IPv6 alone
//!   so  [::]:p  and  0.0.0.0:p  both bind whenever p is free in both families
//! ```
//!
//! Left to the system default, an IPv6 socket is dual-stack on Linux (`net.ipv6.bindv6only =
//! 0`): `[::]:p` holds IPv4 port `p` as well, and a later bind of `0.0.0.0:p` fails with
//! `EADDRINUSE`.

use std::io;
use std::net::SocketAddr;

use socket2::Domain;
use socket2::Protocol;
use socket2::Socket;
use socket2::Type;
use tokio::net::TcpListener;
use tokio::net::UdpSocket;

/// Connections a listener's kernel queue holds before they are accepted, as many as Tokio's own
/// listeners hold.
const LISTEN_BACKLOG: i32 = 1_024;

/// A UDP socket bound to `address`.
pub(crate) fn bind_udp(address: SocketAddr) -> io::Result<UdpSocket> {
    let socket = socket_for(address, Type::DGRAM, Protocol::UDP)?;
    socket.bind(&address.into())?;
    UdpSocket::from_std(std::net::UdpSocket::from(socket))
}

/// A TCP listener on `address`, with the address reusable as Tokio's own listeners make it.
pub(crate) fn listen_tcp(address: SocketAddr) -> io::Result<TcpListener> {
    let socket = socket_for(address, Type::STREAM, Protocol::TCP)?;
    socket.set_reuse_address(true)?;
    socket.bind(&address.into())?;
    socket.listen(LISTEN_BACKLOG)?;
    TcpListener::from_std(std::net::TcpListener::from(socket))
}

/// An unbound, non-blocking socket of `kind` in `address`'s family, IPv6 only when that family
/// is IPv6.
fn socket_for(address: SocketAddr, kind: Type, protocol: Protocol) -> io::Result<Socket> {
    let socket = Socket::new(Domain::for_address(address), kind, Some(protocol))?;
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    Ok(socket)
}
