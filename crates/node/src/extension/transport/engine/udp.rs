#![warn(missing_docs)]
//! UDP instance of the relay: the listener (client side) and the per-flow datagram relay
//! loops (server side, plus the client-side return path). UDP flows have no half-close.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::inject_track;
use super::open;
use super::send_frame;
use super::Outbound;
use super::RelayTask;
use super::TransportSessions;
use super::UDP_BUF;
use crate::extension::ext::Core;
use crate::extension::transport::Frame;
use crate::extension::transport::SessionKey;

impl TransportSessions {
    /// Bind a UDP socket; per new source address open a flow (`Track` + `Frame::Open`) and
    /// relay its datagrams, routing replies back to the originating local client.
    pub(super) async fn listen_udp(
        self: Arc<Self>,
        core: Core,
        local_addr: SocketAddr,
        peer: Did,
        service: String,
        namespace: String,
    ) {
        let socket = match UdpSocket::bind(local_addr).await {
            Ok(socket) => Arc::new(socket),
            Err(e) => {
                tracing::error!("transport udp bind {local_addr} failed: {e:?}");
                return;
            }
        };
        tokio::spawn(async move {
            let mut flows: HashMap<SocketAddr, SessionKey> = HashMap::new();
            let mut buf = vec![0u8; UDP_BUF];
            loop {
                match socket.recv_from(buf.as_mut_slice()).await {
                    Ok((n, src)) => {
                        let bytes = Bytes::copy_from_slice(buf.get(..n).unwrap_or_default());
                        // Reuse the flow only if its key is still live: a peer `Close` or
                        // relay teardown removes the key from the engine table but cannot
                        // reach this listener-local `flows` map, so a stale mapping would
                        // otherwise route new datagrams to a dead (un-deliverable) session.
                        let key = match flows.get(&src) {
                            Some(key) if self.is_live(key) => key.clone(),
                            _ => {
                                let key =
                                    SessionKey::new(peer, namespace.clone(), self.next_session());
                                // Register + start the local sender *before* opening, so a
                                // fast reply from the peer is not dropped.
                                let (outbound_rx, cancel) = self.register(key.clone());
                                spawn_udp_sendto(socket.clone(), src, outbound_rx, cancel);
                                // Record the client-side flow in the pure relay state.
                                inject_track(&core, peer, namespace.as_str(), key.session).await;
                                if open(&core, &key, service.as_str()).await.is_err() {
                                    self.close(&core, &key).await;
                                    flows.remove(&src);
                                    continue;
                                }
                                // Overwrites any stale mapping for this source.
                                flows.insert(src, key.clone());
                                key
                            }
                        };
                        let frame = Frame::Data {
                            session: key.session,
                            bytes,
                        };
                        let _ = send_frame(&core, key.peer, key.namespace.as_str(), frame).await;
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
        core,
        key,
        mut outbound_rx,
        cancel,
    } = task;
    let peer = key.peer;
    let session = key.session;
    let namespace = key.namespace.clone();
    let mut buf = vec![0u8; UDP_BUF];
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            received = socket.recv(buf.as_mut_slice()) => match received {
                Ok(n) => {
                    let bytes = Bytes::copy_from_slice(buf.get(..n).unwrap_or_default());
                    if send_frame(&core, peer, namespace.as_str(), Frame::Data {
                        session,
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
    sessions.close(&core, &key).await;
    let _ = send_frame(&core, peer, namespace.as_str(), Frame::Close { session }).await;
}

/// Client-side UDP flow: route peer bytes back to the originating local client `dest`.
fn spawn_udp_sendto(
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
