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

use super::open;
use super::send_frame;
use super::Outbound;
use super::RelayTask;
use super::TransportSessions;
use super::UDP_BUF;
use crate::backend::transport::Frame;
use crate::backend::transport::SessionKey;
use crate::processor::Processor;

impl TransportSessions {
    /// Bind a UDP socket; per new source address open a flow (`Frame::Open`) and relay its
    /// datagrams, routing replies back to the originating local client.
    pub(super) async fn listen_udp(
        self: Arc<Self>,
        processor: Arc<Processor>,
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
                        let key = match flows.get(&src) {
                            Some(key) => key.clone(),
                            None => {
                                let key =
                                    SessionKey::new(peer, namespace.clone(), self.next_session());
                                // Register + start the local sender *before* opening, so a
                                // fast reply from the peer is not dropped.
                                let (outbound_rx, cancel) = self.register(key.clone());
                                spawn_udp_sendto(socket.clone(), src, outbound_rx, cancel);
                                if open(processor.as_ref(), &key, service.as_str())
                                    .await
                                    .is_err()
                                {
                                    self.close(&key);
                                    continue;
                                }
                                flows.insert(src, key.clone());
                                key
                            }
                        };
                        let frame = Frame::Data {
                            session: key.session,
                            bytes,
                        };
                        let _ =
                            send_frame(processor.as_ref(), key.peer, key.namespace.as_str(), frame)
                                .await;
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
        processor,
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
                    if send_frame(processor.as_ref(), peer, namespace.as_str(), Frame::Data {
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
    sessions.close(&key);
    let _ = send_frame(processor.as_ref(), peer, namespace.as_str(), Frame::Close {
        session,
    })
    .await;
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
