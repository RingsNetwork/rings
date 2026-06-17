#![warn(missing_docs)]
//! Native transport-relay engine — the imperative shell that owns live sockets.
//!
//! This is the side-effecting half of the relay (the pure half is each transport's
//! `Protocol::step`). It keys live OS resources by [`SessionId`] in a shared table and
//! is driven only through the transport [`Effect`](crate::backend::ext::Effect)s
//! (`Connect` / `Write` / `Close`). Local reads are sent back to the peer as
//! [`Frame`]s over the overlay — the event trace flowing outward.
//!
//! Session lifecycle (stream transports):
//!
//! ```text
//!   Connect ─▶ [ local TcpStream ]
//!      local read  n>0  ─▶ send Frame::Data{session, bytes} to peer
//!      local read  0/err ─▶ send Frame::Close{session}; drop session
//!      Write{session}    ─▶ write bytes to local stream
//!      Close{session}    ─▶ cancel + drop session
//! ```

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use rings_core::dht::Did;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::backend::ext::Envelope;
use crate::backend::transport::Frame;
use crate::backend::transport::SessionId;
use crate::backend::types::BackendMessage;
use crate::error::Error;
use crate::error::Result;
use crate::processor::Processor;

/// Connect timeout for a local service dial.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Local read buffer size.
const READ_BUF: usize = 30_000;

/// A live relayed session: the write side of the local stream plus a cancel token.
struct SessionHandle {
    write_tx: mpsc::Sender<Bytes>,
    cancel: CancellationToken,
}

/// Shared table of live sessions. Held by the provider and shared into each
/// interpreter, so `Connect` (which opens a session) and a later `Write`/`Close`
/// (which address it) see the same resources.
#[derive(Default)]
pub struct TransportSessions {
    map: Mutex<HashMap<SessionId, SessionHandle>>,
}

impl TransportSessions {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a local TCP connection to `addr` for `session`, relaying its byte stream
    /// to `peer` under `namespace`. Local reads are sent as `Frame::Data`; peer writes
    /// arrive via [`write`](Self::write). On connect failure or EOF a `Frame::Close`
    /// is sent and the session is dropped.
    pub async fn connect(
        &self,
        processor: Arc<Processor>,
        session: SessionId,
        peer: Did,
        namespace: String,
        addr: SocketAddr,
    ) {
        let stream = match timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => stream,
            _ => {
                let _ = send_frame(processor.as_ref(), peer, namespace.as_str(), Frame::Close {
                    session,
                })
                .await;
                return;
            }
        };

        let (write_tx, mut write_rx) = mpsc::channel::<Bytes>(1024);
        let cancel = CancellationToken::new();
        self.insert(session, SessionHandle {
            write_tx,
            cancel: cancel.clone(),
        });

        let sessions_namespace = namespace.clone();
        tokio::spawn(async move {
            let (mut local_read, mut local_write) = stream.into_split();

            let read_to_peer = async {
                let mut buf = vec![0u8; READ_BUF];
                loop {
                    match local_read.read(buf.as_mut_slice()).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let bytes = Bytes::copy_from_slice(buf.get(..n).unwrap_or_default());
                            let frame = Frame::Data { session, bytes };
                            if send_frame(
                                processor.as_ref(),
                                peer,
                                sessions_namespace.as_str(),
                                frame,
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            };

            let peer_to_local = async {
                while let Some(bytes) = write_rx.recv().await {
                    if local_write.write_all(bytes.as_ref()).await.is_err() {
                        break;
                    }
                }
            };

            tokio::select! {
                _ = read_to_peer => {}
                _ = peer_to_local => {}
                _ = cancel.cancelled() => {}
            }

            let _ = send_frame(
                processor.as_ref(),
                peer,
                sessions_namespace.as_str(),
                Frame::Close { session },
            )
            .await;
        });
    }

    /// Write peer-originated bytes to a session's local stream. Unknown sessions are
    /// dropped silently (the session may have already closed).
    pub async fn write(&self, session: SessionId, bytes: Bytes) {
        let tx = self
            .map
            .lock()
            .ok()
            .and_then(|map| map.get(&session).map(|handle| handle.write_tx.clone()));
        if let Some(tx) = tx {
            let _ = tx.send(bytes).await;
        }
    }

    /// Close and drop a session.
    pub fn close(&self, session: SessionId) {
        if let Ok(mut map) = self.map.lock() {
            if let Some(handle) = map.remove(&session) {
                handle.cancel.cancel();
            }
        }
    }

    fn insert(&self, session: SessionId, handle: SessionHandle) {
        if let Ok(mut map) = self.map.lock() {
            map.insert(session, handle);
        }
    }
}

/// Send a [`Frame`] to `peer` under `namespace` over the overlay.
/// `send_frame : (Did, Namespace, Frame) → IO ()`.
///
/// Transitional: the envelope is wrapped in `BackendMessage::Envelope` so the current
/// receiver (which decodes `BackendMessage` first) can route it.
async fn send_frame(processor: &Processor, peer: Did, namespace: &str, frame: Frame) -> Result<()> {
    let payload = bincode::serialize(&frame).map_err(|_| Error::EncodeError)?;
    let envelope = Envelope::new(namespace.to_string(), Bytes::from(payload));
    processor
        .send_backend_message(peer, BackendMessage::Envelope(envelope))
        .await?;
    Ok(())
}
