#![warn(missing_docs)]
//! UDP instance of the relay: the listener (client side) and the per-flow datagram relay
//! loops (server side, plus the client-side return path). UDP flows have no half-close.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::inject_accepted;
use super::send_frame;
use super::Outbound;
use super::Pending;
use super::RelayTask;
use super::TransportSessions;
use super::UDP_BUF;
use crate::extension::ext::Scope;
use crate::extension::transport::Frame;

impl TransportSessions {
    /// Bind a UDP socket; route each datagram. A datagram from a **known** source (in the
    /// effect-populated `udp_flows` cache) is forwarded directly (fast path, no `step`). A
    /// datagram from a **new** source stashes the first bytes and reports the accept to the
    /// pure relay (`Accepted`); the core mints the session id and replies with `OpenAccepted`
    /// → [`bind_accepted`](TransportSessions::bind_accepted), which opens the flow and
    /// forwards that first datagram. The listener picks no identity.
    pub(super) async fn listen_udp(
        self: Arc<Self>,
        scope: Scope,
        local_addr: SocketAddr,
        peer: Did,
        service: String,
    ) {
        let socket = match UdpSocket::bind(local_addr).await {
            Ok(socket) => Arc::new(socket),
            Err(e) => {
                tracing::error!("transport udp bind {local_addr} failed: {e:?}");
                return;
            }
        };
        tokio::spawn(async move {
            let mut buf = vec![0u8; UDP_BUF];
            loop {
                match socket.recv_from(buf.as_mut_slice()).await {
                    Ok((n, src)) => {
                        let bytes = Bytes::copy_from_slice(buf.get(..n).unwrap_or_default());
                        match self.udp_flow(&src) {
                            Some(key) => {
                                let _ = send_frame(&scope, key.peer, Frame::Data {
                                    session: key.session,
                                    from_opener: super::opened_by_us(&key),
                                    bytes,
                                })
                                .await;
                            }
                            None => {
                                if let Some(token) = self.stash_pending(Pending::Udp {
                                    socket: socket.clone(),
                                    src,
                                    first: bytes,
                                }) {
                                    inject_accepted(&scope, token, peer, service.clone()).await;
                                    // Drop the stashed flow if the round-trip didn't bind it.
                                    self.evict_pending(token);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("transport udp recv on {local_addr} failed: {e:?}");
                        break;
                    }
                }
            }
        });
    }
}

/// Server-side UDP flow: a per-flow socket connected to the backend.
pub(super) async fn relay_udp_connected(task: RelayTask, socket: UdpSocket) {
    let RelayTask {
        sessions,
        scope,
        key,
        mut outbound_rx,
        cancel,
        generation,
    } = task;
    let peer = key.peer;
    let session = key.session;
    let from_opener = super::opened_by_us(&key);
    let mut buf = vec![0u8; UDP_BUF];
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            received = socket.recv(buf.as_mut_slice()) => match received {
                Ok(n) => {
                    let bytes = Bytes::copy_from_slice(buf.get(..n).unwrap_or_default());
                    if send_frame(&scope, peer, Frame::Data {
                        session,
                        from_opener,
                        bytes,
                    })
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            },
            outbound = outbound_rx.recv() => match outbound {
                Some(Outbound::Data(bytes)) => {
                    let _ = socket.send(bytes.as_ref()).await;
                }
                Some(Outbound::Shutdown) | None => break,
            },
        }
    }
    // Only tell the peer if we were still the current owner (stale task stays silent).
    if sessions.close_if_current(&scope, &key, generation).await {
        let _ = send_frame(&scope, peer, Frame::Close {
            session,
            from_opener,
        })
        .await;
    }
}

/// Client-side UDP flow: route peer bytes back to the originating local client `dest`.
pub(super) fn spawn_udp_sendto(
    socket: Arc<UdpSocket>,
    dest: SocketAddr,
    mut outbound_rx: mpsc::Receiver<Outbound>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                outbound = outbound_rx.recv() => match outbound {
                    Some(Outbound::Data(bytes)) => {
                        let _ = socket.send_to(bytes.as_ref(), dest).await;
                    }
                    Some(Outbound::Shutdown) | None => break,
                },
            }
        }
    });
}

/// Bind an ephemeral UDP socket and connect it to `addr`.
pub(super) async fn bind_connected_udp(addr: SocketAddr) -> Option<UdpSocket> {
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().ok()?
    } else {
        "[::]:0".parse().ok()?
    };
    let socket = UdpSocket::bind(bind).await.ok()?;
    socket.connect(addr).await.ok()?;
    Some(socket)
}
