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
use crate::extension::ext::Scope;
use crate::extension::transport::Frame;

impl TransportSessions {
    /// Bind a TCP listener; per accepted connection, stash the stream and report the accept
    /// to the pure relay (`Accepted`). The core mints the session id and replies with
    /// `OpenAccepted` → [`bind_accepted`](TransportSessions::bind_accepted), which opens the
    /// peer session and starts the relay loop. The listener itself decides nothing.
    pub(super) async fn listen_tcp(
        self: Arc<Self>,
        scope: Scope,
        local_addr: SocketAddr,
        peer: Did,
        service: String,
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
                        if inject_accepted(&scope, token, peer, service.clone())
                            .await
                            .is_err()
                        {
                            self.evict_pending(token);
                        }
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
        scope,
        key,
        mut outbound_rx,
        cancel,
        generation,
    } = task;
    let peer = key.peer;
    let session = key.session;
    let from_opener = super::opened_by_us(&key);
    let (mut local_read, mut local_write) = stream.into_split();

    // local → peer; clean EOF sends FIN, errors abort the whole session.
    let local_to_peer = {
        let scope = scope.clone();
        let cancel = cancel.clone();
        async move {
            let mut buf = vec![0u8; TCP_BUF];
            loop {
                match local_read.read(buf.as_mut_slice()).await {
                    Ok(0) => {
                        let _ = send_frame(&scope, peer, Frame::Shutdown {
                            session,
                            from_opener,
                        })
                        .await;
                        break;
                    }
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

    // Teardown: drop *our* session instance (generation-checked, so we never delete a newer
    // reuse of the key) — which `Untrack`s it from the pure state. Only tell the peer if we
    // were still the current owner; a stale task must not Close the peer's reused session.
    if sessions.close_if_current(&scope, &key, generation).await {
        let _ = send_frame(&scope, peer, Frame::Close {
            session,
            from_opener,
        })
        .await;
    }
}
