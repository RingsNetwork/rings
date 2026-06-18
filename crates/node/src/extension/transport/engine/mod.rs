#![warn(missing_docs)]
//! Native transport-relay engine — the imperative shell that owns live sockets.
//!
//! The pure half of the relay is `Relay::step`; this is the side-effecting half (the relay
//! extension's interpreter). It keys live OS resources by `SessionKey` and is driven by the
//! relay's own `RelayEffect`s (`Connect`/`Write`/`Shutdown`/`Close`) plus the `listen` entry
//! point. Local reads flow back to the peer as [`Frame`]s (the event trace flowing outward).
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
//! ## Module layout
//!
//! This `mod.rs` is the shared scaffolding — the session table, the open/write/close
//! plumbing, and the per-session `RelayTask`. The two transport instances live beside
//! it (TCP/UDP are one abstraction at two points of the session-cardinality axis):
//!
//! - `tcp` — the TCP listener and the bidirectional byte-stream relay loop.
//! - `udp` — the UDP listener and the per-flow datagram relay loop.
//!
//! ## v1 limits
//!
//! A relayed datagram must fit one overlay message (`UDP_BUF`; larger is truncated);
//! UDP flows are not yet idle-GC'd; reliable-tunnelled UDP does not preserve native
//! loss/reorder semantics.

mod tcp;
mod udp;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use rings_core::dht::Did;
use tokio::net::TcpStream;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Core;
use crate::extension::protocols::relay::RelayCommand;
use crate::extension::transport::Frame;
use crate::extension::transport::SessionKey;
use crate::extension::transport::TransportKind;

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

/// A live relayed session: the peer→local channel plus a cancel token. `src` is the local
/// UDP client address (for cleaning up the `udp_flows` cache on close); `None` for TCP.
struct SessionHandle {
    outbound: mpsc::Sender<Outbound>,
    cancel: CancellationToken,
    src: Option<SocketAddr>,
}

/// A locally-accepted connection/flow that has been reported to the pure relay (`Accepted`)
/// and is waiting for the core to mint its session key (`OpenAccepted` → `bind_accepted`).
enum Pending {
    /// An accepted TCP connection.
    Tcp(TcpStream),
    /// A new UDP flow: the shared listener socket, the local client address, and the first
    /// datagram (carried so it isn't lost during the round-trip to the core).
    Udp {
        socket: Arc<UdpSocket>,
        src: SocketAddr,
        first: Bytes,
    },
}

/// Shared resource tables for the relay. The engine **mints nothing about protocol identity**
/// (no session ids, no routing): it allocates engine-local `token`s for pending accepts and
/// holds caches that are populated by the pure relay's effects.
///
/// - `map`: live sessions keyed by [`SessionKey`] (the core-minted identity). The bare
///   opener `SessionId` is not a valid key — keying by the authenticated `peer` is what makes
///   a frame unable to address another peer's session.
/// - `pending`: accepted-but-not-yet-bound connections/flows, keyed by engine-local token.
/// - `udp_flows`: `src → SessionKey` fast-path cache for the UDP data plane, populated by
///   [`bind_accepted`](TransportSessions::bind_accepted) — a projection of the core's decision,
///   never an independent source of identity.
#[derive(Default)]
pub struct TransportSessions {
    map: Mutex<HashMap<SessionKey, SessionHandle>>,
    tokens: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    udp_flows: Mutex<HashMap<SocketAddr, SessionKey>>,
}

impl TransportSessions {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh engine-local token for a pending accept (not a session id — the core
    /// mints session ids).
    fn next_token(&self) -> u64 {
        self.tokens.fetch_add(1, Ordering::Relaxed)
    }

    /// Server side. Open a local backend for `session` and relay to `peer` under
    /// `namespace`. The session handle is registered *before* the (async) dial, so
    /// `Data` arriving during connect is buffered rather than dropped. On failure a
    /// `Frame::Close` is sent and the session removed.
    pub async fn connect(
        self: Arc<Self>,
        core: Core,
        key: SessionKey,
        addr: SocketAddr,
        kind: TransportKind,
    ) {
        let task = RelayTask::register(self.clone(), core, key);
        tokio::spawn(async move {
            match kind {
                TransportKind::Tcp => {
                    match timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
                        Ok(Ok(stream)) => tcp::relay_tcp(task, stream).await,
                        _ => task.refuse().await,
                    }
                }
                TransportKind::Udp => match udp::bind_connected_udp(addr).await {
                    Some(socket) => udp::relay_udp_connected(task, socket).await,
                    None => task.refuse().await,
                },
            }
        });
    }

    /// Client side. Bind a local listener; per accepted TCP connection / new UDP
    /// source assign a session, send `Frame::Open{session, service}`, and relay it.
    pub async fn listen(
        self: Arc<Self>,
        core: Core,
        local_addr: SocketAddr,
        peer: Did,
        service: String,
        namespace: String,
        kind: TransportKind,
    ) {
        match kind {
            TransportKind::Tcp => {
                self.listen_tcp(core, local_addr, peer, service, namespace)
                    .await
            }
            TransportKind::Udp => {
                self.listen_udp(core, local_addr, peer, service, namespace)
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

    /// Fully close and drop a session, then feed the teardown back to the pure protocol as
    /// an `Untrack` (so `State.sessions` is the full authority). Injects exactly once — only
    /// when a live handle was actually removed.
    pub async fn close(&self, core: &Core, key: &SessionKey) {
        let removed = self.map.lock().ok().and_then(|mut map| map.remove(key));
        if let Some(handle) = removed {
            handle.cancel.cancel();
            // Drop the UDP fast-path cache entry for this session, if any.
            if let Some(src) = handle.src {
                if let Ok(mut flows) = self.udp_flows.lock() {
                    flows.remove(&src);
                }
            }
            inject_untrack(core, key).await;
        }
    }

    /// Bind a pending accepted connection/flow (engine-local `token`) to the session `key`
    /// the pure relay just minted (the `OpenAccepted` effect): register the handle, send
    /// `Frame::Open`, and start relaying. The engine never chose the id — it only reported
    /// the raw accept and now executes the core's decision.
    pub async fn bind_accepted(
        self: Arc<Self>,
        core: Core,
        token: u64,
        key: SessionKey,
        service: String,
    ) {
        let Some(pending) = self.pending.lock().ok().and_then(|mut p| p.remove(&token)) else {
            return; // listener gone or token already consumed
        };
        match pending {
            Pending::Tcp(stream) => {
                let task = RelayTask::register(self.clone(), core.clone(), key.clone());
                if open(&core, &key, service.as_str()).await.is_err() {
                    task.refuse().await;
                    return;
                }
                tokio::spawn(async move { tcp::relay_tcp(task, stream).await });
            }
            Pending::Udp { socket, src, first } => {
                let (outbound_rx, cancel) = self.register(key.clone(), Some(src));
                if let Ok(mut flows) = self.udp_flows.lock() {
                    flows.insert(src, key.clone());
                }
                udp::spawn_udp_sendto(socket, src, outbound_rx, cancel);
                if open(&core, &key, service.as_str()).await.is_err() {
                    self.close(&core, &key).await;
                    return;
                }
                // Forward the first datagram that triggered this flow.
                let _ = send_frame(&core, key.peer, key.namespace.as_str(), Frame::Data {
                    session: key.session,
                    bytes: first,
                })
                .await;
            }
        }
    }

    /// Look up the live session for a UDP source (fast-path data plane; the cache is populated
    /// by [`bind_accepted`](TransportSessions::bind_accepted)).
    fn udp_flow(&self, src: &SocketAddr) -> Option<SessionKey> {
        let key = self.udp_flows.lock().ok()?.get(src).cloned()?;
        self.is_live(&key).then_some(key)
    }

    /// Stash a pending accept under a fresh engine-local token (for the round-trip to the
    /// core, which mints the id and replies with `OpenAccepted`).
    fn stash_pending(&self, pending: Pending) -> Option<u64> {
        let token = self.next_token();
        self.pending.lock().ok()?.insert(token, pending);
        Some(token)
    }

    // ── shared ───────────────────────────────────────────────────────────────────

    /// Create a session's channel + cancel token and record its handle, returning the
    /// receiver and cancel for the relay task. `src` is the local UDP client address (for
    /// fast-path cache cleanup) or `None` for TCP.
    fn register(
        &self,
        key: SessionKey,
        src: Option<SocketAddr>,
    ) -> (mpsc::Receiver<Outbound>, CancellationToken) {
        let (outbound, outbound_rx) = mpsc::channel::<Outbound>(1024);
        let cancel = CancellationToken::new();
        self.insert(key, SessionHandle {
            outbound,
            cancel: cancel.clone(),
            src,
        });
        (outbound_rx, cancel)
    }

    fn sender(&self, key: &SessionKey) -> Option<mpsc::Sender<Outbound>> {
        self.map
            .lock()
            .ok()
            .and_then(|map| map.get(key).map(|handle| handle.outbound.clone()))
    }

    /// Whether a session is currently live (registered in the table). Used by the UDP
    /// listener to detect a flow whose key has since been closed.
    fn is_live(&self, key: &SessionKey) -> bool {
        self.map
            .lock()
            .map(|map| map.contains_key(key))
            .unwrap_or(false)
    }

    fn insert(&self, key: SessionKey, handle: SessionHandle) {
        if let Ok(mut map) = self.map.lock() {
            // Defensive: if a handle already exists for this key (a duplicate Open that
            // slipped past the pure reject, or a key reuse), cancel the old relay task
            // before replacing it, so it cannot keep running or later tear down the new one.
            if let Some(old) = map.insert(key, handle) {
                old.cancel.cancel();
            }
        }
    }
}

/// Everything a per-session relay task needs: the engine handle, the session's routing
/// identity, and its peer→local channel + cancel token. Bundling these keeps the relay
/// task signatures to `(task, socket)`.
struct RelayTask {
    sessions: Arc<TransportSessions>,
    core: Core,
    key: SessionKey,
    outbound_rx: mpsc::Receiver<Outbound>,
    cancel: CancellationToken,
}

impl RelayTask {
    /// Register a fresh session channel on the engine and capture the routing identity.
    fn register(sessions: Arc<TransportSessions>, core: Core, key: SessionKey) -> Self {
        let (outbound_rx, cancel) = sessions.register(key.clone(), None);
        Self {
            sessions,
            core,
            key,
            outbound_rx,
            cancel,
        }
    }

    /// Connect failed: drop the pre-registered session (which `Untrack`s it) and tell the peer.
    async fn refuse(self) {
        self.sessions.close(&self.core, &self.key).await;
        let _ = send_frame(
            &self.core,
            self.key.peer,
            self.key.namespace.as_str(),
            Frame::Close {
                session: self.key.session,
            },
        )
        .await;
    }
}

/// Send `Frame::Open` to the session's peer (client side, on a new local connection/flow).
async fn open(core: &Core, key: &SessionKey, service: &str) -> Result<()> {
    send_frame(core, key.peer, key.namespace.as_str(), Frame::Open {
        session: key.session,
        service: service.to_string(),
    })
    .await
}

/// Send a [`Frame`] to `peer` under `namespace` over the overlay.
async fn send_frame(core: &Core, peer: Did, namespace: &str, frame: Frame) -> Result<()> {
    let payload = bincode::serialize(&frame).map_err(|_| Error::EncodeError)?;
    core.send(peer, namespace, Bytes::from(payload)).await
}

/// Report a client-side accept to the pure relay (which mints the session id and replies
/// with `OpenAccepted`). The engine passes only its local `token` — it picks no identity.
async fn inject_accepted(core: &Core, token: u64, peer: Did, namespace: &str, service: String) {
    let command = RelayCommand::<SocketAddr>::Accepted {
        token,
        peer,
        service,
    };
    if let Ok(bytes) = bincode::serialize(&command) {
        let _ = core.inject(namespace, Bytes::from(bytes)).await;
    }
}

/// Feed a teardown back to the pure relay so it removes the session from `State.sessions`.
async fn inject_untrack(core: &Core, key: &SessionKey) {
    let command = RelayCommand::<SocketAddr>::Untrack {
        peer: key.peer,
        session: key.session,
    };
    if let Ok(bytes) = bincode::serialize(&command) {
        let _ = core
            .inject(key.namespace.as_str(), Bytes::from(bytes))
            .await;
    }
}
