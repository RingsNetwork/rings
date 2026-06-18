#![warn(missing_docs)]
//! TCP instance of the relay: the listener (client side) and the bidirectional
//! byte-stream relay loop (server side), with true half-close and abrupt-close handling.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use super::inject_accepted;
use super::send_frame;
use super::Outbound;
use super::Pending;
use super::RelayTask;
use super::TransportSessions;
use super::TCP_BUF;
use crate::extension::ext::Core;
use crate::extension::transport::Frame;

impl TransportSessions {
    /// Bind a TCP listener; per accepted connection, stash the stream and report the accept
    /// to the pure relay (`Accepted`). The core mints the session id and replies with
    /// `OpenAccepted` → [`bind_accepted`](TransportSessions::bind_accepted), which opens the
    /// peer session and starts the relay loop. The listener itself decides nothing.
    pub(super) async fn listen_tcp(
        self: Arc<Self>,
        core: Core,
        local_addr: SocketAddr,
        peer: Did,
        service: String,
        namespace: String,
    ) {
        let listener = match TcpListener::bind(local_addr).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::error!("transport listen bind {local_addr} failed: {e:?}");
                return;
            }
        };
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let Some(token) = self.stash_pending(Pending::Tcp(stream)) else {
                            continue;
                        };
                        inject_accepted(&core, token, peer, namespace.as_str(), service.clone())
                            .await;
                    }
                    Err(e) => {
                        tracing::error!("transport accept on {local_addr} failed: {e:?}");
                        break;
                    }
                }
            }
        });
    }
}

/// Bidirectional TCP relay with true half-close and abrupt-close handling.
pub(super) async fn relay_tcp(task: RelayTask, stream: TcpStream) {
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
    let (mut local_read, mut local_write) = stream.into_split();

    // local → peer; clean EOF sends FIN, errors abort the whole session.
    let local_to_peer = {
        let core = core.clone();
        let namespace = namespace.clone();
        let cancel = cancel.clone();
        async move {
            let mut buf = vec![0u8; TCP_BUF];
            loop {
                match local_read.read(buf.as_mut_slice()).await {
                    Ok(0) => {
                        let _ = send_frame(&core, peer, namespace.as_str(), Frame::Shutdown {
                            session,
                        })
                        .await;
                        break;
                    }
                    Ok(n) => {
                        let bytes = Bytes::copy_from_slice(buf.get(..n).unwrap_or_default());
                        if send_frame(&core, peer, namespace.as_str(), Frame::Data {
                            session,
                            bytes,
                        })
                        .await
                        .is_err()
                        {
                            cancel.cancel(); // overlay unreachable → abrupt
                            break;
                        }
                    }
                    Err(_) => {
                        cancel.cancel(); // local read error → abrupt
                        break;
                    }
                }
            }
        }
    };

    // peer → local; FIN shuts the write side, write errors abort.
    let peer_to_local = {
        let cancel = cancel.clone();
        async move {
            while let Some(outbound) = outbound_rx.recv().await {
                match outbound {
                    Outbound::Data(bytes) => {
                        if local_write.write_all(bytes.as_ref()).await.is_err() {
                            cancel.cancel();
                            break;
                        }
                    }
                    Outbound::Shutdown => {
                        let _ = local_write.shutdown().await;
                        break;
                    }
                }
            }
        }
    };

    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = async { tokio::join!(local_to_peer, peer_to_local); } => {}
    }

    // Teardown: drop the session (which `Untrack`s it from the pure state) and tell the peer.
    sessions.close(&core, &key).await;
    let _ = send_frame(&core, peer, namespace.as_str(), Frame::Close { session }).await;
}
