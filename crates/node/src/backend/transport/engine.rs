#![warn(missing_docs)]
//! Native transport-relay engine — the imperative shell that owns live sockets.
//!
//! The pure half of the relay is each transport's `Protocol::step`; this is the
//! side-effecting half. It keys live OS resources by [`SessionId`] and is driven only
//! through the transport [`Effect`](crate::backend::ext::Effect)s
//! (`Connect`/`Listen`/`Write`/`Shutdown`/`Close`). Local reads flow back to the peer
//! as [`Frame`]s (the event trace flowing outward).
//!
//! ## Lifecycle, half-close and abrupt close
//!
//! A TCP session is full-duplex; each direction ends independently:
//!
//! ```text
//!   local read = Ok(0)   (clean EOF)  ─▶ Frame::Shutdown (FIN); reverse stays open
//!   peer Frame::Shutdown               ─▶ shutdown local write; forward stays open
//!   both directions done               ─▶ Frame::Close; drop session
//!
//!   local read/write error, or overlay send failure (abrupt)
//!                                      ─▶ cancel the whole session ─▶ Frame::Close;
//!                                         drop session  (RST-like)
//!   peer Frame::Close                  ─▶ cancel + drop session
//! ```
//!
//! So a half-closing peer (request fully sent, awaiting response) does not deadlock,
//! and an abrupt drop (RST / dead overlay) tears the whole session down on both ends
//! rather than leaking it. UDP flows have no half-close; `Shutdown` is ignored and
//! errors close the flow.
//!
//! ## v1 limits
//!
//! A relayed datagram must fit one overlay message (`UDP_BUF`; larger is truncated);
//! UDP flows are not yet idle-GC'd; reliable-tunnelled UDP does not preserve native
//! loss/reorder semantics.

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
use crate::backend::transport::SessionKey;
use crate::backend::transport::TransportKind;
use crate::error::Error;
use crate::error::Result;
use crate::processor::Processor;

/// Connect timeout for a local service dial.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Local TCP read buffer size.
const TCP_BUF: usize = 30_000;
/// Local UDP datagram buffer (one datagram per frame; larger is truncated, v1).
const UDP_BUF: usize = 65_536;

/// Something to deliver to a session's local socket (peer → local direction).
enum Outbound {
    /// Bytes to write/send locally.
    Data(Bytes),
    /// The peer half-closed (FIN): shut the local write side.
    Shutdown,
}

/// A live relayed session: the peer→local channel plus a cancel token.
struct SessionHandle {
    outbound: mpsc::Sender<Outbound>,
    cancel: CancellationToken,
}

/// Shared table of live sessions plus the session-id allocator.
///
/// Sessions are keyed by [`SessionKey`] (`peer, namespace, session`), not by the bare
/// opener-assigned [`SessionId`]: the id is only unique within one `(peer, namespace)`, and
/// keying by the authenticated `peer` is what makes a frame unable to address another peer's
/// session (a mismatched key simply misses the lookup).
#[derive(Default)]
pub struct TransportSessions {
    map: Mutex<HashMap<SessionKey, SessionHandle>>,
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

    /// Server side. Open a local backend for `session` and relay to `peer` under
    /// `namespace`. The session handle is registered *before* the (async) dial, so
    /// `Data` arriving during connect is buffered rather than dropped. On failure a
    /// `Frame::Close` is sent and the session removed.
    pub async fn connect(
        self: Arc<Self>,
        processor: Arc<Processor>,
        key: SessionKey,
        addr: SocketAddr,
        kind: TransportKind,
    ) {
        let task = RelayTask::register(self.clone(), processor, key);
        tokio::spawn(async move {
            match kind {
                TransportKind::Tcp => {
                    match timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
                        Ok(Ok(stream)) => relay_tcp(task, stream).await,
                        _ => task.refuse().await,
                    }
                }
                TransportKind::Udp => match bind_connected_udp(addr).await {
                    Some(socket) => relay_udp_connected(task, socket).await,
                    None => task.refuse().await,
                },
            }
        });
    }

    /// Client side. Bind a local listener; per accepted TCP connection / new UDP
    /// source assign a session, send `Frame::Open{session, service}`, and relay it.
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

    /// Deliver peer bytes to a session's local socket. Unknown sessions are dropped — and a
    /// non-owner peer's key never resolves, so it cannot write to a session it does not own.
    pub async fn write(&self, key: &SessionKey, bytes: Bytes) {
        if let Some(tx) = self.sender(key) {
            let _ = tx.send(Outbound::Data(bytes)).await;
        }
    }

    /// Half-close a session's local write side (peer sent FIN).
    pub async fn shutdown(&self, key: &SessionKey) {
        if let Some(tx) = self.sender(key) {
            let _ = tx.send(Outbound::Shutdown).await;
        }
    }

    /// Fully close and drop a session.
    pub fn close(&self, key: &SessionKey) {
        if let Ok(mut map) = self.map.lock() {
            if let Some(handle) = map.remove(key) {
                handle.cancel.cancel();
            }
        }
    }

    // ── TCP listen ─────────────────────────────────────────────────────────────

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
                        let key = SessionKey::new(peer, namespace.clone(), self.next_session());
                        // Register the local handle *before* telling the peer to open, so
                        // an early `Data`/`Close` from the peer is buffered, not dropped.
                        let task =
                            RelayTask::register(self.clone(), processor.clone(), key.clone());
                        if open(processor.as_ref(), &key, service.as_str())
                            .await
                            .is_err()
                        {
                            task.refuse().await;
                            continue;
                        }
                        tokio::spawn(async move { relay_tcp(task, stream).await });
                    }
                    Err(e) => {
                        tracing::error!("transport accept on {local_addr} failed: {e:?}");
                        break;
                    }
                }
            }
        });
    }

    // ── UDP listen ─────────────────────────────────────────────────────────────

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

    // ── shared ───────────────────────────────────────────────────────────────────

    /// Create a session's channel + cancel token and record its handle, returning the
    /// receiver and cancel for the relay task.
    fn register(&self, key: SessionKey) -> (mpsc::Receiver<Outbound>, CancellationToken) {
        let (outbound, outbound_rx) = mpsc::channel::<Outbound>(1024);
        let cancel = CancellationToken::new();
        self.insert(key, SessionHandle {
            outbound,
            cancel: cancel.clone(),
        });
        (outbound_rx, cancel)
    }

    fn sender(&self, key: &SessionKey) -> Option<mpsc::Sender<Outbound>> {
        self.map
            .lock()
            .ok()
            .and_then(|map| map.get(key).map(|handle| handle.outbound.clone()))
    }

    fn insert(&self, key: SessionKey, handle: SessionHandle) {
        if let Ok(mut map) = self.map.lock() {
            map.insert(key, handle);
        }
    }
}

/// Everything a per-session relay task needs: the engine handle, the session's routing
/// identity, and its peer→local channel + cancel token. Bundling these keeps the relay
/// task signatures to `(task, socket)`.
struct RelayTask {
    sessions: Arc<TransportSessions>,
    processor: Arc<Processor>,
    key: SessionKey,
    outbound_rx: mpsc::Receiver<Outbound>,
    cancel: CancellationToken,
}

impl RelayTask {
    /// Register a fresh session channel on the engine and capture the routing identity.
    fn register(
        sessions: Arc<TransportSessions>,
        processor: Arc<Processor>,
        key: SessionKey,
    ) -> Self {
        let (outbound_rx, cancel) = sessions.register(key.clone());
        Self {
            sessions,
            processor,
            key,
            outbound_rx,
            cancel,
        }
    }

    /// Connect failed: drop the pre-registered session and tell the peer.
    async fn refuse(self) {
        self.sessions.close(&self.key);
        let _ = send_frame(
            self.processor.as_ref(),
            self.key.peer,
            self.key.namespace.as_str(),
            Frame::Close {
                session: self.key.session,
            },
        )
        .await;
    }
}

/// Bidirectional TCP relay with true half-close and abrupt-close handling.
async fn relay_tcp(task: RelayTask, stream: TcpStream) {
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
    let (mut local_read, mut local_write) = stream.into_split();

    // local → peer; clean EOF sends FIN, errors abort the whole session.
    let local_to_peer = {
        let processor = processor.clone();
        let namespace = namespace.clone();
        let cancel = cancel.clone();
        async move {
            let mut buf = vec![0u8; TCP_BUF];
            loop {
                match local_read.read(buf.as_mut_slice()).await {
                    Ok(0) => {
                        let _ = send_frame(
                            processor.as_ref(),
                            peer,
                            namespace.as_str(),
                            Frame::Shutdown { session },
                        )
                        .await;
                        break;
                    }
                    Ok(n) => {
                        let bytes = Bytes::copy_from_slice(buf.get(..n).unwrap_or_default());
                        if send_frame(processor.as_ref(), peer, namespace.as_str(), Frame::Data {
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

    // Teardown: drop the session and tell the peer (idempotent on its side).
    sessions.close(&key);
    let _ = send_frame(processor.as_ref(), peer, namespace.as_str(), Frame::Close {
        session,
    })
    .await;
}

/// Server-side UDP flow: a per-flow socket connected to the backend.
async fn relay_udp_connected(task: RelayTask, socket: UdpSocket) {
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

/// Send `Frame::Open` to the session's peer (client side, on a new local connection/flow).
async fn open(processor: &Processor, key: &SessionKey, service: &str) -> Result<()> {
    send_frame(processor, key.peer, key.namespace.as_str(), Frame::Open {
        session: key.session,
        service: service.to_string(),
    })
    .await
}

/// Send a [`Frame`] to `peer` under `namespace` over the overlay (as a bare
/// [`Envelope`]).
async fn send_frame(processor: &Processor, peer: Did, namespace: &str, frame: Frame) -> Result<()> {
    let payload = bincode::serialize(&frame).map_err(|_| Error::EncodeError)?;
    let envelope = Envelope::new(namespace.to_string(), Bytes::from(payload));
    processor.send_envelope(peer, &envelope).await?;
    Ok(())
}
