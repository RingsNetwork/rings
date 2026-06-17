#![warn(missing_docs)]
//! Native transport-relay engine — the imperative shell that owns live sockets.
//!
//! The pure half of the relay is each transport's `Protocol::step`; this is the
//! side-effecting half. It keys live OS resources by [`SessionId`] in a shared table
//! and is driven only through the transport [`Effect`](crate::backend::ext::Effect)s
//! (`Connect` / `Write` / `Close` / `Listen`). Local reads are sent back to the peer as
//! [`Frame`]s over the overlay — the event trace flowing outward.
//!
//! One [`SessionHandle`] abstraction (an mpsc write side + a cancel token) serves both
//! [`TransportKind`]s and both relay directions; only how the local socket is obtained
//! and read/written differs:
//!
//! ```text
//!   kind  direction  open                         local→peer        peer→local
//!   TCP   Connect    dial TcpStream               read  → Data      Write → write
//!   TCP   Listen     accept TcpStream             read  → Data      Write → write
//!   UDP   Connect    bind+connect UdpSocket       recv  → Data      Write → send
//!   UDP   Listen     bind UdpSocket, demux by src recvfrom→Data     Write → send_to(src)
//! ```
//!
//! Limits (v1): a relayed datagram must fit one overlay message ([`UDP_BUF`]); larger
//! datagrams are truncated. UDP flows are not yet idle-GC'd. Tunnelling UDP over the
//! reliable overlay yields "reliable-tunnelled UDP" — it does not preserve native
//! loss/reorder semantics (unsuitable for e.g. QUIC).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use rings_core::dht::Did;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::backend::ext::Envelope;
use crate::backend::transport::Frame;
use crate::backend::transport::SessionId;
use crate::backend::transport::TransportKind;
use crate::backend::types::BackendMessage;
use crate::error::Error;
use crate::error::Result;
use crate::processor::Processor;

/// Connect timeout for a local service dial.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Local TCP read buffer size.
const TCP_BUF: usize = 30_000;
/// Local UDP datagram buffer (one datagram per frame; larger is truncated, v1).
const UDP_BUF: usize = 65_536;

/// A live relayed session: the write side of the local socket plus a cancel token.
struct SessionHandle {
    write_tx: mpsc::Sender<Bytes>,
    cancel: CancellationToken,
}

/// Shared table of live sessions plus the session-id allocator. Held by the provider
/// and shared into each interpreter, so `Connect`/`Listen` (which open sessions) and a
/// later `Write`/`Close` (which address them) see the same resources.
#[derive(Default)]
pub struct TransportSessions {
    map: Mutex<HashMap<SessionId, SessionHandle>>,
    counter: AtomicU64,
}

impl TransportSessions {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh session id (unique within this node).
    fn next_session(&self) -> SessionId {
        SessionId(self.counter.fetch_add(1, Ordering::Relaxed))
    }

    /// Server side. Open a local backend for `session` (dial TCP, or bind+connect a
    /// UDP socket) and relay to `peer` under `namespace`. On failure a `Frame::Close`
    /// is sent.
    pub async fn connect(
        &self,
        processor: Arc<Processor>,
        session: SessionId,
        peer: Did,
        namespace: String,
        addr: SocketAddr,
        kind: TransportKind,
    ) {
        match kind {
            TransportKind::Tcp => match timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
                Ok(Ok(stream)) => self.spawn_tcp(processor, session, peer, namespace, stream),
                _ => {
                    self.refuse(processor.as_ref(), session, peer, namespace.as_str())
                        .await
                }
            },
            TransportKind::Udp => match bind_connected_udp(addr).await {
                Some(socket) => {
                    self.spawn_udp_connected(processor, session, peer, namespace, socket)
                }
                None => {
                    self.refuse(processor.as_ref(), session, peer, namespace.as_str())
                        .await
                }
            },
        }
    }

    /// Client side. Bind a local listener; for each accepted TCP connection / new UDP
    /// source assign a session, send `Frame::Open{session, service}` to `peer`, and
    /// relay it under `namespace`.
    pub async fn listen(
        self: Arc<Self>,
        processor: Arc<Processor>,
        local_addr: SocketAddr,
        peer: Did,
        service: String,
        namespace: String,
        kind: TransportKind,
    ) {
        match kind {
            TransportKind::Tcp => {
                self.listen_tcp(processor, local_addr, peer, service, namespace)
                    .await
            }
            TransportKind::Udp => {
                self.listen_udp(processor, local_addr, peer, service, namespace)
                    .await
            }
        }
    }

    /// Write peer-originated bytes to a session's local socket. Unknown sessions are
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

    // ── TCP ────────────────────────────────────────────────────────────────────

    async fn listen_tcp(
        self: Arc<Self>,
        processor: Arc<Processor>,
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
                        let session = self.next_session();
                        if self
                            .open(
                                processor.as_ref(),
                                session,
                                peer,
                                namespace.as_str(),
                                service.as_str(),
                            )
                            .await
                            .is_err()
                        {
                            continue;
                        }
                        self.spawn_tcp(processor.clone(), session, peer, namespace.clone(), stream);
                    }
                    Err(e) => {
                        tracing::error!("transport accept on {local_addr} failed: {e:?}");
                        break;
                    }
                }
            }
        });
    }

    /// Register a local TCP `stream` as `session` and spawn its bidirectional relay.
    fn spawn_tcp(
        &self,
        processor: Arc<Processor>,
        session: SessionId,
        peer: Did,
        namespace: String,
        stream: TcpStream,
    ) {
        let (mut write_rx, _tx, cancel) = self.register(session);
        tokio::spawn(async move {
            let (mut local_read, mut local_write) = stream.into_split();

            let read_to_peer = async {
                let mut buf = vec![0u8; TCP_BUF];
                loop {
                    match local_read.read(buf.as_mut_slice()).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let bytes = Bytes::copy_from_slice(buf.get(..n).unwrap_or_default());
                            let frame = Frame::Data { session, bytes };
                            if send_frame(processor.as_ref(), peer, namespace.as_str(), frame)
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
            let _ = send_frame(processor.as_ref(), peer, namespace.as_str(), Frame::Close {
                session,
            })
            .await;
        });
    }

    // ── UDP ────────────────────────────────────────────────────────────────────

    async fn listen_udp(
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
            let mut flows: HashMap<SocketAddr, SessionId> = HashMap::new();
            let mut buf = vec![0u8; UDP_BUF];
            loop {
                match socket.recv_from(buf.as_mut_slice()).await {
                    Ok((n, src)) => {
                        let bytes = Bytes::copy_from_slice(buf.get(..n).unwrap_or_default());
                        let session = match flows.get(&src) {
                            Some(session) => *session,
                            None => {
                                let session = self.next_session();
                                flows.insert(src, session);
                                if self
                                    .open(
                                        processor.as_ref(),
                                        session,
                                        peer,
                                        namespace.as_str(),
                                        service.as_str(),
                                    )
                                    .await
                                    .is_err()
                                {
                                    continue;
                                }
                                self.spawn_udp_sendto(session, socket.clone(), src);
                                session
                            }
                        };
                        let frame = Frame::Data { session, bytes };
                        let _ =
                            send_frame(processor.as_ref(), peer, namespace.as_str(), frame).await;
                    }
                    Err(e) => {
                        tracing::error!("transport udp recv on {local_addr} failed: {e:?}");
                        break;
                    }
                }
            }
        });
    }

    /// Server side UDP: a per-flow socket connected to the backend. Local recv → Data;
    /// peer Write → send.
    fn spawn_udp_connected(
        &self,
        processor: Arc<Processor>,
        session: SessionId,
        peer: Did,
        namespace: String,
        socket: UdpSocket,
    ) {
        let (mut write_rx, _tx, cancel) = self.register(session);
        tokio::spawn(async move {
            let mut buf = vec![0u8; UDP_BUF];
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    received = socket.recv(buf.as_mut_slice()) => match received {
                        Ok(n) => {
                            let bytes = Bytes::copy_from_slice(buf.get(..n).unwrap_or_default());
                            let frame = Frame::Data { session, bytes };
                            if send_frame(processor.as_ref(), peer, namespace.as_str(), frame)
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    outbound = write_rx.recv() => match outbound {
                        Some(bytes) => {
                            let _ = socket.send(bytes.as_ref()).await;
                        }
                        None => break,
                    },
                }
            }
            let _ = send_frame(processor.as_ref(), peer, namespace.as_str(), Frame::Close {
                session,
            })
            .await;
        });
    }

    /// Client side UDP: route peer Write for `session` back to the originating local
    /// client `dest` on the shared bound `socket`.
    fn spawn_udp_sendto(&self, session: SessionId, socket: Arc<UdpSocket>, dest: SocketAddr) {
        let (mut write_rx, _tx, cancel) = self.register(session);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    outbound = write_rx.recv() => match outbound {
                        Some(bytes) => {
                            let _ = socket.send_to(bytes.as_ref(), dest).await;
                        }
                        None => break,
                    },
                }
            }
        });
    }

    // ── shared ───────────────────────────────────────────────────────────────────

    /// Create a session's channel + cancel token and record its handle, returning the
    /// receiver and cancel for the relay task.
    fn register(
        &self,
        session: SessionId,
    ) -> (
        mpsc::Receiver<Bytes>,
        mpsc::Sender<Bytes>,
        CancellationToken,
    ) {
        let (write_tx, write_rx) = mpsc::channel::<Bytes>(1024);
        let cancel = CancellationToken::new();
        self.insert(session, SessionHandle {
            write_tx: write_tx.clone(),
            cancel: cancel.clone(),
        });
        (write_rx, write_tx, cancel)
    }

    fn insert(&self, session: SessionId, handle: SessionHandle) {
        if let Ok(mut map) = self.map.lock() {
            map.insert(session, handle);
        }
    }

    /// Send `Frame::Open` to `peer` (client side, on a new local connection/flow).
    async fn open(
        &self,
        processor: &Processor,
        session: SessionId,
        peer: Did,
        namespace: &str,
        service: &str,
    ) -> Result<()> {
        send_frame(processor, peer, namespace, Frame::Open {
            session,
            service: service.to_string(),
        })
        .await
    }

    /// Send `Frame::Close` to `peer` (server side, on connect failure).
    async fn refuse(&self, processor: &Processor, session: SessionId, peer: Did, namespace: &str) {
        let _ = send_frame(processor, peer, namespace, Frame::Close { session }).await;
    }
}

/// Bind an ephemeral UDP socket and connect it to `addr` so `send`/`recv` target the
/// backend.
async fn bind_connected_udp(addr: SocketAddr) -> Option<UdpSocket> {
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().ok()?
    } else {
        "[::]:0".parse().ok()?
    };
    let socket = UdpSocket::bind(bind).await.ok()?;
    socket.connect(addr).await.ok()?;
    Some(socket)
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
