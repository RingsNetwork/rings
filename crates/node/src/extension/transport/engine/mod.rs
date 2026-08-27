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
//! A relayed datagram must fit one overlay message (`UDP_BUF`; larger is truncated).
//! Reliable-tunnelled UDP does not preserve native loss/reorder semantics.

mod tcp;
mod udp;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
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
use crate::extension::ext::Scope;
use crate::extension::protocols::relay::RelayCommand;
use crate::extension::transport::allocate_non_reusing;
use crate::extension::transport::platform::spawn_detached;
use crate::extension::transport::EffectEnqueue;
use crate::extension::transport::Frame;
use crate::extension::transport::Initiator;
use crate::extension::transport::SessionKey;
use crate::extension::transport::TransportKind;

/// Connect timeout for a local service dial.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Local TCP read buffer size.
const TCP_BUF: usize = 30_000;
/// Local UDP datagram buffer (one datagram per frame; larger is truncated, v1).
const UDP_BUF: usize = 65_536;
/// Accepted local resources awaiting the reducer's session-id decision.
const MAX_PENDING_ACCEPTS: usize = 128;

/// Something to deliver to a session's local socket (peer → local direction).
enum Outbound {
    /// Bytes to write/send locally.
    Data(Bytes),
    /// The peer half-closed (FIN): shut the local write side.
    Shutdown,
}

/// A live relayed session: the peer→local channel plus a cancel token. `src` is the local
/// UDP client address (for cleaning up the `udp_flows` cache on close); `None` for TCP.
/// `generation` is a per-insert stamp: a relay task only tears down the handle whose
/// generation matches its own, so a slow old task can never delete a newer reuse of the same
/// key (ABA safety).
struct SessionHandle {
    outbound: mpsc::Sender<Outbound>,
    cancel: CancellationToken,
    src: Option<SocketAddr>,
    generation: u64,
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

/// Native UDP source lifecycle. Only `Active` is visible to the local→peer fast path.
///
/// ```text
/// Vacant --reserve(token)--> Pending(token) --bind(key)--> Opening(key)
/// Opening(key) --Open + first Data enqueued, generation current--> Active(key)
/// Pending --inject failure--> Vacant
/// Opening|Active --session teardown--> Vacant
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
enum UdpFlowState {
    Pending(u64),
    Opening(SessionKey),
    Active(SessionKey),
}

impl UdpFlowState {
    fn has_pending_token(&self, token: u64) -> bool {
        matches!(self, Self::Pending(actual) if *actual == token)
    }

    fn is_opening(&self, key: &SessionKey) -> bool {
        matches!(self, Self::Opening(actual) if actual == key)
    }

    fn belongs_to(&self, key: &SessionKey) -> bool {
        matches!(self, Self::Opening(actual) | Self::Active(actual) if actual == key)
    }
}

/// Shared resource tables for the relay. The engine **mints nothing about protocol identity**
/// (no session ids, no routing): it allocates engine-local `token`s for pending accepts and
/// holds caches that are populated by the pure relay's effects.
///
/// - `map`: live sessions keyed by [`SessionKey`] (the core-minted identity). The bare
///   opener `SessionId` is not a valid key — keying by the authenticated `peer` is what makes
///   a frame unable to address another peer's session.
/// - `pending`: accepted-but-not-yet-bound connections/flows, keyed by engine-local token.
/// - `udp_flows`: `src → Pending | Opening | Active` projection. It suppresses duplicate accepts
///   while the core mints a key, and exposes a fast-path key only after `Frame::Open` is admitted.
#[derive(Default)]
pub(crate) struct TransportSessions {
    map: Mutex<HashMap<SessionKey, SessionHandle>>,
    tokens: AtomicU64,
    generations: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    udp_flows: Mutex<HashMap<SocketAddr, UdpFlowState>>,
}

impl TransportSessions {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh engine-local token for a pending accept (not a session id — the core
    /// mints session ids).
    fn next_token(&self) -> Option<u64> {
        allocate_non_reusing(&self.tokens)
    }

    /// Server side. Open a local backend for `session` and relay to `peer` under
    /// `namespace`. The session handle is registered *before* the (async) dial, so
    /// `Data` arriving during connect is buffered rather than dropped. On failure a
    /// `Frame::Close` is sent and the session removed.
    pub fn connect(
        self: Arc<Self>,
        scope: Scope,
        key: SessionKey,
        addr: SocketAddr,
        kind: TransportKind,
    ) -> EffectEnqueue {
        debug_assert_eq!(
            scope.namespace(),
            key.namespace.as_str(),
            "relay engine acted with a scope outside the session's namespace"
        );
        let Some(task) = RelayTask::register(self.clone(), scope, key) else {
            return EffectEnqueue::Failed;
        };
        spawn_detached(async move {
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
        EffectEnqueue::Enqueued
    }

    /// Client side. Bind a local listener; per accepted TCP connection / new UDP
    /// source assign a session, send `Frame::Open{session, service}`, and relay it.
    pub async fn listen(
        self: Arc<Self>,
        scope: Scope,
        local_addr: SocketAddr,
        peer: Did,
        service: String,
        kind: TransportKind,
    ) {
        match kind {
            TransportKind::Tcp => self.listen_tcp(scope, local_addr, peer, service).await,
            TransportKind::Udp => self.listen_udp(scope, local_addr, peer, service).await,
        }
    }

    /// Client side. Relay an already-accepted TCP stream to `peer`'s `service`.
    pub async fn relay_tcp_stream(
        self: Arc<Self>,
        scope: Scope,
        stream: TcpStream,
        peer: Did,
        service: String,
    ) {
        let Some(token) = self.stash_pending(Pending::Tcp(stream)) else {
            return;
        };
        if inject_accepted(&scope, token, peer, service).await.is_err() {
            self.evict_pending(token);
        }
    }

    /// Enqueue peer bytes without waiting for local-socket backpressure. Returns whether the
    /// pure relay must forget the session. A full channel is a terminal local resource failure:
    /// removing it bounds memory and prevents one stalled socket from holding the namespace's
    /// ordered transition gate.
    pub fn write(&self, key: &SessionKey, bytes: Bytes) -> EffectEnqueue {
        self.enqueue_for_effect(key, Outbound::Data(bytes))
    }

    /// Half-close a session's local write side (peer sent FIN).
    pub fn shutdown(&self, key: &SessionKey) -> EffectEnqueue {
        self.enqueue_for_effect(key, Outbound::Shutdown)
    }

    /// Release a session while applying a `Close` effect.
    ///
    /// The pure reducer has already removed `key` for this effect, so feeding `Untrack` back is
    /// redundant and would recursively enter the currently active ordered effect turn.
    pub fn close_for_effect(&self, key: &SessionKey) {
        let removed = self.map.lock().ok().and_then(|mut map| map.remove(key));
        self.finish_close_without_feedback(key, removed);
    }

    /// Close a session **only if** its handle still has `generation` — so a slow old relay
    /// task tearing down can never drop a newer reuse of the same key (ABA safety). Returns
    /// whether it was the current owner (and thus removed it); a stale task gets `false` and
    /// must therefore *also* not send the peer a `Close` (which would tear down the peer's
    /// reused session).
    async fn close_if_current(&self, scope: &Scope, key: &SessionKey, generation: u64) -> bool {
        let removed = self.remove_if_current(key, generation);
        self.finish_close(scope, key, removed).await
    }

    fn remove_if_current(&self, key: &SessionKey, generation: u64) -> Option<SessionHandle> {
        self.map.lock().ok().and_then(|mut map| {
            let current = map.get(key).map(|handle| handle.generation);
            (current == Some(generation))
                .then(|| map.remove(key))
                .flatten()
        })
    }

    /// Shared teardown tail: cancel the task, drop the UDP cache entry, and `Untrack` — but
    /// only if a handle was actually removed (exactly-once). Returns whether it removed one.
    async fn finish_close(
        &self,
        scope: &Scope,
        key: &SessionKey,
        removed: Option<SessionHandle>,
    ) -> bool {
        let Some(handle) = removed else {
            return false;
        };
        handle.cancel.cancel();
        if let Some(src) = handle.src {
            self.remove_udp_flow_for_key(src, key);
        }
        inject_untrack(scope, key).await;
        true
    }

    /// Shared resource cleanup for a synchronous interpreter feedback path.
    ///
    /// Post: if this returns `true`, the OS resource is gone and the caller owns the matching
    /// `RelayCommand::Untrack` feedback obligation.
    fn finish_close_without_feedback(
        &self,
        key: &SessionKey,
        removed: Option<SessionHandle>,
    ) -> bool {
        let Some(handle) = removed else {
            return false;
        };
        handle.cancel.cancel();
        if let Some(src) = handle.src {
            self.remove_udp_flow_for_key(src, key);
        }
        true
    }

    /// Bind a pending accepted connection/flow (engine-local `token`) to the session `key`
    /// the pure relay just minted (the `OpenAccepted` effect).
    ///
    /// Registration is synchronous and returns any immediate `Untrack` obligation to the active
    /// reducer turn. The peer-facing `Open` and subsequent relay run on a lifecycle task, so a
    /// peer-controlled data-channel send cannot suspend the namespace transition gate.
    pub fn bind_accepted(
        self: Arc<Self>,
        scope: Scope,
        token: u64,
        key: SessionKey,
        service: String,
    ) -> Option<SessionKey> {
        debug_assert_eq!(
            scope.namespace(),
            key.namespace.as_str(),
            "relay engine bound a session under a foreign namespace scope"
        );
        let Some(pending) = self.pending.lock().ok().and_then(|mut p| p.remove(&token)) else {
            // The reducer already recorded `key`; synchronously return its cleanup obligation.
            return Some(key);
        };
        match pending {
            Pending::Tcp(stream) => {
                let Some(task) = RelayTask::register(self.clone(), scope.clone(), key.clone())
                else {
                    return Some(key);
                };
                spawn_detached(async move {
                    if open(&task.scope, &task.key, service.as_str())
                        .await
                        .is_err()
                    {
                        task.refuse().await;
                        return;
                    }
                    match task.sessions.current_generation(&task.key) {
                        Some(generation) if generation == task.generation => {
                            tcp::relay_tcp(task, stream).await;
                        }
                        None => cancel_open(&task.scope, &task.key).await,
                        Some(_) => {}
                    }
                });
            }
            Pending::Udp { socket, src, first } => {
                if !self.promote_udp_flow(src, token, &key) {
                    return Some(key);
                }
                let Some((outbound_rx, cancel, generation)) = self.register(key.clone(), Some(src))
                else {
                    self.remove_udp_flow_for_key(src, &key);
                    return Some(key);
                };
                udp::spawn_udp_sendto(
                    RelayTask {
                        sessions: Arc::clone(&self),
                        scope: scope.clone(),
                        key: key.clone(),
                        outbound_rx,
                        cancel,
                        generation,
                    },
                    socket,
                    src,
                );
                spawn_detached(async move {
                    if open(&scope, &key, service.as_str()).await.is_err() {
                        if self.close_if_current(&scope, &key, generation).await {
                            let _ = send_frame(&scope, key.peer, Frame::Close {
                                session: key.session,
                                from_opener: opened_by_us(&key),
                            })
                            .await;
                        }
                        return;
                    }
                    match self.current_generation(&key) {
                        Some(current) if current == generation => {}
                        None => {
                            cancel_open(&scope, &key).await;
                            return;
                        }
                        Some(_) => return,
                    }
                    // Preserve local receive order: keep the fast path closed until both Open and
                    // the first datagram have been admitted to the peer connection.
                    let from_opener = opened_by_us(&key);
                    if send_frame(&scope, key.peer, Frame::Data {
                        session: key.session,
                        from_opener,
                        bytes: first,
                    })
                    .await
                    .is_err()
                    {
                        self.close_if_current(&scope, &key, generation).await;
                        return;
                    }
                    if !self.activate_udp_flow(src, &key)
                        && self.close_if_current(&scope, &key, generation).await
                    {
                        cancel_open(&scope, &key).await;
                    }
                });
            }
        }
        None
    }

    /// Look up the live session for a UDP source (fast-path data plane; the cache is populated
    /// by [`bind_accepted`](TransportSessions::bind_accepted)).
    fn udp_flow(&self, src: &SocketAddr) -> Option<SessionKey> {
        let key = match self.udp_flows.lock().ok()?.get(src) {
            Some(UdpFlowState::Active(key)) => key.clone(),
            Some(UdpFlowState::Pending(_) | UdpFlowState::Opening(_)) | None => return None,
        };
        if self.is_live(&key) {
            return Some(key);
        }
        self.remove_udp_flow_for_key(*src, &key);
        None
    }

    /// Reserve a UDP source and its pending token atomically. A second datagram for the same
    /// source is deliberately dropped until the first flow becomes active; it cannot create a
    /// second core session or overtake that flow's `Open`.
    fn reserve_pending_udp(
        &self,
        socket: Arc<UdpSocket>,
        src: SocketAddr,
        first: Bytes,
    ) -> Option<u64> {
        let mut flows = self.udp_flows.lock().ok()?;
        if flows.contains_key(&src) {
            return None;
        }
        let mut pending_accepts = self.pending.lock().ok()?;
        if pending_accepts.len() >= MAX_PENDING_ACCEPTS {
            return None;
        }
        let token = self.next_token()?;
        pending_accepts.insert(token, Pending::Udp { socket, src, first });
        flows.insert(src, UdpFlowState::Pending(token));
        Some(token)
    }

    fn promote_udp_flow(&self, src: SocketAddr, token: u64, key: &SessionKey) -> bool {
        let Ok(mut flows) = self.udp_flows.lock() else {
            return false;
        };
        let Some(state) = flows.get_mut(&src) else {
            return false;
        };
        if !state.has_pending_token(token) {
            return false;
        }
        *state = UdpFlowState::Opening(key.clone());
        true
    }

    fn activate_udp_flow(&self, src: SocketAddr, key: &SessionKey) -> bool {
        let Ok(mut flows) = self.udp_flows.lock() else {
            return false;
        };
        let Some(state) = flows.get_mut(&src) else {
            return false;
        };
        if !state.is_opening(key) {
            return false;
        }
        *state = UdpFlowState::Active(key.clone());
        true
    }

    fn remove_udp_flow_for_token(&self, src: SocketAddr, token: u64) {
        if let Ok(mut flows) = self.udp_flows.lock() {
            let remove = flows
                .get(&src)
                .is_some_and(|state| state.has_pending_token(token));
            if remove {
                flows.remove(&src);
            }
        }
    }

    fn remove_udp_flow_for_key(&self, src: SocketAddr, key: &SessionKey) {
        if let Ok(mut flows) = self.udp_flows.lock() {
            let remove = flows.get(&src).is_some_and(|state| state.belongs_to(key));
            if remove {
                flows.remove(&src);
            }
        }
    }

    /// Stash a pending accept under a fresh engine-local token (for the round-trip to the
    /// core, which mints the id and replies with `OpenAccepted`).
    fn stash_pending(&self, pending: Pending) -> Option<u64> {
        let mut pending_accepts = self.pending.lock().ok()?;
        if pending_accepts.len() >= MAX_PENDING_ACCEPTS {
            return None;
        }
        let token = self.next_token()?;
        pending_accepts.insert(token, pending);
        Some(token)
    }

    /// Drop a still-unbound pending accept (its socket/stream), so a failed
    /// `Accepted` inject — decode reject, dispatch error — can't leak the resource.
    /// A no-op once `bind_accepted` has consumed the token.
    fn evict_pending(&self, token: u64) {
        let removed = self
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&token));
        if let Some(Pending::Udp { src, .. }) = removed {
            self.remove_udp_flow_for_token(src, token);
        }
    }

    /// Release a local accept rejected synchronously by the pure relay allocator.
    pub(crate) fn evict_pending_for_effect(&self, token: u64) {
        self.evict_pending(token);
    }

    // ── shared ───────────────────────────────────────────────────────────────────

    /// Create a session's channel + cancel token and record its handle, returning the
    /// receiver and cancel for the relay task. `src` is the local UDP client address (for
    /// fast-path cache cleanup) or `None` for TCP.
    fn register(
        &self,
        key: SessionKey,
        src: Option<SocketAddr>,
    ) -> Option<(mpsc::Receiver<Outbound>, CancellationToken, u64)> {
        let (outbound, outbound_rx) = mpsc::channel::<Outbound>(1024);
        let cancel = CancellationToken::new();
        let generation = allocate_non_reusing(&self.generations)?;
        self.insert(key, SessionHandle {
            outbound,
            cancel: cancel.clone(),
            src,
            generation,
        });
        Some((outbound_rx, cancel, generation))
    }

    fn sender(&self, key: &SessionKey) -> Option<(mpsc::Sender<Outbound>, u64)> {
        self.map.lock().ok().and_then(|map| {
            map.get(key)
                .map(|handle| (handle.outbound.clone(), handle.generation))
        })
    }

    fn current_generation(&self, key: &SessionKey) -> Option<u64> {
        self.map
            .lock()
            .ok()
            .and_then(|map| map.get(key).map(|handle| handle.generation))
    }

    /// Imperative admission boundary for the bounded peer→local queue.
    ///
    /// Post: [`EffectEnqueue::Failed`] means this effect removed the exact generation whose
    /// queue rejected the operation; [`EffectEnqueue::Missing`] means no generation existed.
    /// An ABA-replaced generation is never removed.
    fn enqueue_for_effect(&self, key: &SessionKey, outbound: Outbound) -> EffectEnqueue {
        let Some((sender, generation)) = self.sender(key) else {
            return EffectEnqueue::Missing;
        };
        match sender.try_send(outbound) {
            Ok(()) => EffectEnqueue::Enqueued,
            Err(_) => {
                if self.finish_close_without_feedback(key, self.remove_if_current(key, generation))
                {
                    EffectEnqueue::Failed
                } else {
                    EffectEnqueue::Enqueued
                }
            }
        }
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
    scope: Scope,
    key: SessionKey,
    outbound_rx: mpsc::Receiver<Outbound>,
    cancel: CancellationToken,
    generation: u64,
}

impl RelayTask {
    /// Register a fresh session channel on the engine and capture the routing identity.
    fn register(sessions: Arc<TransportSessions>, scope: Scope, key: SessionKey) -> Option<Self> {
        let (outbound_rx, cancel, generation) = sessions.register(key.clone(), None)?;
        Some(Self {
            sessions,
            scope,
            key,
            outbound_rx,
            cancel,
            generation,
        })
    }

    /// Connect failed: drop the pre-registered session (which `Untrack`s it) and tell the peer
    /// — but only if we were still the current owner (a stale task stays silent).
    async fn refuse(self) {
        if self
            .sessions
            .close_if_current(&self.scope, &self.key, self.generation)
            .await
        {
            let _ = send_frame(&self.scope, self.key.peer, Frame::Close {
                session: self.key.session,
                from_opener: opened_by_us(&self.key),
            })
            .await;
        }
    }
}

#[cfg(test)]
fn relay_task_for_test(namespace: &str) -> Result<(RelayTask, Arc<TransportSessions>, SessionKey)> {
    relay_task_for_test_with_src(namespace, None)
}

#[cfg(test)]
fn relay_task_for_test_with_src(
    namespace: &str,
    src: Option<SocketAddr>,
) -> Result<(RelayTask, Arc<TransportSessions>, SessionKey)> {
    use rings_core::ecc::SecretKey;
    use rings_core::session::SessionSk;

    use crate::extension::ext::Extensions;
    use crate::processor::ProcessorBuilder;
    use crate::processor::ProcessorConfig;

    let session_sk = SessionSk::new_with_seckey(&SecretKey::random())?;
    let config = ProcessorConfig::new(1, String::new(), session_sk, 1);
    let processor = ProcessorBuilder::from_config(&config)?
        .advertise_presence(false)
        .build()?;
    let extensions = Extensions::new(Arc::new(processor));
    let scope = Scope::new(extensions.core(), namespace.to_string());
    let sessions = Arc::new(TransportSessions::new());
    let initiator = if src.is_some() {
        Initiator::Local
    } else {
        Initiator::Remote
    };
    let key = SessionKey::new(
        Did::from(99_u32),
        namespace,
        crate::extension::transport::SessionId(1),
        initiator,
    );
    let (outbound_rx, cancel, generation) =
        sessions.register(key.clone(), src).ok_or_else(|| {
            Error::ExtensionError("test relay generation exhausted unexpectedly".to_string())
        })?;
    let task = RelayTask {
        sessions: Arc::clone(&sessions),
        scope,
        key: key.clone(),
        outbound_rx,
        cancel,
        generation,
    };
    Ok((task, sessions, key))
}

/// Whether *this node* opened the session (sets `Frame::from_opener` on outbound frames).
fn opened_by_us(key: &SessionKey) -> bool {
    matches!(key.initiator, Initiator::Local)
}

/// Send `Frame::Open` to the session's peer (client side, on a new local connection/flow).
async fn open(scope: &Scope, key: &SessionKey, service: &str) -> Result<()> {
    send_frame(scope, key.peer, Frame::Open {
        session: key.session,
        service: service.to_string(),
    })
    .await
}

/// Compensate an `Open` that reached the transport after its local generation was removed.
/// The send happens after `Open` admission, so the per-connection FIFO observes Open → Close.
async fn cancel_open(scope: &Scope, key: &SessionKey) {
    let _ = send_frame(scope, key.peer, Frame::Close {
        session: key.session,
        from_opener: opened_by_us(key),
    })
    .await;
}

/// Send a [`Frame`] to `peer` over the overlay, under the scope's own namespace.
async fn send_frame(scope: &Scope, peer: Did, frame: Frame) -> Result<()> {
    let payload = rings_codec::serialize(&frame).map_err(|_| Error::EncodeError)?;
    scope.send(peer, Bytes::from(payload)).await
}

/// Report a client-side accept to the pure relay (which mints the session id and replies
/// with `OpenAccepted`). The engine passes only its local `token` — it picks no identity.
async fn inject_accepted(scope: &Scope, token: u64, peer: Did, service: String) -> Result<()> {
    let command = RelayCommand::<SocketAddr>::Accepted {
        token,
        peer,
        service,
    };
    let bytes = rings_codec::serialize(&command).map_err(|_| Error::EncodeError)?;
    scope.inject(Bytes::from(bytes)).await
}

/// Feed a teardown back to the pure relay so it removes the session from `State.sessions`.
/// The engine has already dropped the live handle, so a failed inject means the reducer may
/// still list a now-dead session — surface it rather than silently diverging.
async fn inject_untrack(scope: &Scope, key: &SessionKey) {
    let command = RelayCommand::<SocketAddr>::Untrack {
        peer: key.peer,
        session: key.session,
        initiator: key.initiator,
    };
    if let Ok(bytes) = rings_codec::serialize(&command) {
        if let Err(e) = scope.inject(Bytes::from(bytes)).await {
            tracing::warn!(
                "relay Untrack inject failed for {key:?}: {e:?}; pure state may still list \
                 this (now dropped) session"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use bytes::Bytes;
    use rings_core::dht::Did;
    use tokio::net::UdpSocket;

    use super::Pending;
    use super::TransportSessions;
    use super::MAX_PENDING_ACCEPTS;
    use crate::extension::transport::EffectEnqueue;
    use crate::extension::transport::Initiator;
    use crate::extension::transport::SessionId;
    use crate::extension::transport::SessionKey;

    #[test]
    fn test_saturated_local_queue_fails_closed_without_waiting() {
        let sessions = TransportSessions::new();
        let key = SessionKey::new(Did::from(7_u32), "tcp", SessionId(11), Initiator::Remote);
        let registration = sessions.register(key.clone(), None);
        assert!(registration.is_some());

        for _ in 0..1024 {
            assert_eq!(
                sessions.write(&key, Bytes::from_static(b"x")),
                EffectEnqueue::Enqueued
            );
        }
        assert_eq!(
            sessions.write(&key, Bytes::from_static(b"overflow")),
            EffectEnqueue::Failed
        );
        assert!(!sessions.is_live(&key));
        assert_eq!(
            sessions.write(&key, Bytes::from_static(b"stale")),
            EffectEnqueue::Missing
        );
    }

    #[tokio::test]
    async fn test_pending_accept_table_rejects_above_its_hard_bound() {
        let sessions = TransportSessions::new();
        let socket = Arc::new(
            UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("bind test UDP socket"),
        );
        for index in 0..MAX_PENDING_ACCEPTS {
            let src = SocketAddr::from(([127, 0, 0, 1], 10_000 + index as u16));
            assert!(sessions
                .stash_pending(Pending::Udp {
                    socket: Arc::clone(&socket),
                    src,
                    first: Bytes::new(),
                })
                .is_some());
        }

        assert!(sessions
            .stash_pending(Pending::Udp {
                socket,
                src: SocketAddr::from(([127, 0, 0, 1], 20_000)),
                first: Bytes::new(),
            })
            .is_none());
        assert_eq!(
            sessions
                .pending
                .lock()
                .expect("pending table remains readable")
                .len(),
            MAX_PENDING_ACCEPTS
        );
    }

    #[tokio::test]
    async fn test_udp_source_is_unique_and_invisible_until_open_admission() {
        let sessions = TransportSessions::new();
        let socket = Arc::new(
            UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("bind test UDP socket"),
        );
        let src = SocketAddr::from(([127, 0, 0, 1], 12_345));
        let token = sessions
            .reserve_pending_udp(Arc::clone(&socket), src, Bytes::from_static(b"first"))
            .expect("reserve first UDP source");

        assert!(sessions
            .reserve_pending_udp(socket, src, Bytes::from_static(b"overtaking"))
            .is_none());
        assert_eq!(sessions.pending.lock().expect("pending table").len(), 1);
        assert_eq!(sessions.udp_flow(&src), None);

        let pending = sessions
            .pending
            .lock()
            .expect("pending table")
            .remove(&token);
        assert!(matches!(pending, Some(Pending::Udp { .. })));
        let key = SessionKey::new(Did::from(8_u32), "udp", SessionId(12), Initiator::Local);
        assert!(sessions.promote_udp_flow(src, token, &key));
        assert!(sessions.register(key.clone(), Some(src)).is_some());
        assert_eq!(sessions.udp_flow(&src), None);

        assert!(sessions.activate_udp_flow(src, &key));
        assert_eq!(sessions.udp_flow(&src), Some(key));
    }

    #[tokio::test]
    async fn test_failed_udp_accept_releases_its_source_reservation() {
        let sessions = TransportSessions::new();
        let socket = Arc::new(
            UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("bind test UDP socket"),
        );
        let src = SocketAddr::from(([127, 0, 0, 1], 12_346));
        let token = sessions
            .reserve_pending_udp(Arc::clone(&socket), src, Bytes::new())
            .expect("reserve first UDP source");

        sessions.evict_pending(token);

        assert!(sessions
            .reserve_pending_udp(socket, src, Bytes::new())
            .is_some());
    }
}
