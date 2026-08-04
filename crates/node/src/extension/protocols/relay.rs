//! Generic transport-relay protocol — one pure server-side state machine for TCP and UDP,
//! native and browser.
//!
//! The pure model is generic over the **target** `T` a service resolves to: a
//! `SocketAddr` natively, a WebTransport `Url` (string) in the browser. The same `step`,
//! state, duplicate-`Open` rejection and owner-rejection serve both — only the
//! *interpreter* differs (native `NativeRelay` over OS sockets, browser `WtRelay` over
//! WebTransport). This is the code realization of "TCP/UDP/native/browser are one relay".
//!
//! Every session is identified by the **owner-scoped key** `(from, namespace, session,
//! initiator)` ([`SessionKey`]). `from` is the authenticated sender (owner rejection: a peer
//! can only name keys whose `from` is itself), and `initiator` records which end opened it —
//! so a session a peer opened never collides with one we opened that got the same id
//! (bidirectional-open safety). A frame's `from_opener` flips to our `initiator`.
//!
//! The reducer is the **sole authority** over the session set: `Data`/`Shutdown`/`Close`
//! emit an effect only for a session in `sessions` (the engine never adjudicates liveness).
//!
//! ```text
//!   S = (services : Name ⇀ T,  sessions : ℘ SessionKey,  next : ℕ)
//!   k = (from, namespace, session, init)        init = Remote if from_opener else Local
//!   step (Command(Register n t))                ↦ (S[services∪{n↦t}], ε)
//!   step (Command(Accepted tok peer svc))       ↦ (S[sessions∪{kₗ}, next+1], [OpenAccepted tok kₗ svc])
//!                                                   where kₗ=(peer,ns,next,Local)   ← core mints the id
//!   step (Command(Untrack k))                   ↦ (S[sessions∖{k}], ε)
//!   step (Frame(from, Open s n)) | k∈sessions   ↦ (S, ε)                            (duplicate)
//!                                | n∈services    ↦ (S∪{k}, [Connect k t kind])
//!                                | otherwise     ↦ (S, [SendClose s])
//!   step (Frame(from, Data s b)) | k∈sessions∖shutdown ↦ (S, [Write k b]) else (S, ε)
//!   step (Frame(from, FIN s))    | TCP ∧ k∈sessions    ↦ (S∪shutdown(k), [Shutdown k])
//!                                                   repeated/UDP ↦ (S, ε)
//!   step (Frame(from, Close s))  | k∈sessions          ↦ (S∖{k}, [Close k]) else (S, ε)
//!   invariant                    |sessions| ≤ 1024 ∧ ∀peer. sessions(peer) ≤ 64
//! ```

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use crate::extension::ext::Ctx;
use crate::extension::ext::EffectScope;
use crate::extension::ext::Interpret;
use crate::extension::ext::MaybeSend;
use crate::extension::ext::Protocol;
use crate::extension::ext::Reject;
use crate::extension::ext::Scope;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;
use crate::extension::transport::EffectEnqueue;
use crate::extension::transport::Frame;
use crate::extension::transport::Initiator;
use crate::extension::transport::SessionId;
use crate::extension::transport::SessionKey;
use crate::extension::transport::TransportKind;

/// Namespace for the TCP relay.
pub const TCP: &str = "tcp";
/// Namespace for the UDP relay.
pub const UDP: &str = "udp";

/// Hard per-namespace live-session bound. The reducer owns admission, so both native and
/// browser interpreters inherit the same resource ceiling.
pub(crate) const MAX_RELAY_SESSIONS: usize = 1_024;
/// A single authenticated peer cannot consume the entire relay-session budget.
pub(crate) const MAX_RELAY_SESSIONS_PER_PEER: usize = 64;

/// A local control command, re-injected by the provider (provenance = self; never sent by
/// peers). Generic over the service target `T`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum RelayCommand<T> {
    /// Map a service name to a local target that `Open` may dial.
    RegisterService {
        /// Service name.
        name: String,
        /// Local target (`SocketAddr` natively, WebTransport URL in browser).
        target: T,
    },
    /// Remove a service mapping.
    UnregisterService {
        /// Service name to remove.
        name: String,
    },
    /// Engine→protocol feedback: a local connection/datagram-flow was accepted, pending
    /// under engine-local `token`, destined for `peer`'s `service`. The pure `step` mints
    /// the session id (so id allocation lives in the core, not the shell), records it, and
    /// replies with [`RelayEffect::OpenAccepted`] to bind the pending resource. The engine
    /// never mints or decides identity — it only reports the raw accept and executes effects.
    Accepted {
        /// Engine-local handle for the pending (not-yet-bound) connection/flow.
        token: u64,
        /// The remote peer this session is tunnelled to.
        peer: Did,
        /// The remote service to open.
        service: String,
    },
    /// Engine→protocol feedback: a session was torn down by the engine (any side); forget
    /// it. The single point through which every teardown reaches the pure state.
    Untrack {
        /// The remote peer of the session.
        peer: Did,
        /// The session id.
        session: SessionId,
        /// Which end opened it (so the right key is removed).
        initiator: Initiator,
    },
    /// Engine→protocol feedback: the current backend failed under an ordered
    /// effect. Forget it and emit exactly one peer-facing `Close`.
    Abort {
        /// The remote peer of the session.
        peer: Did,
        /// The session id.
        session: SessionId,
        /// Which end opened it.
        initiator: Initiator,
    },
}

/// The relay's typed input: a self-injected [`RelayCommand`] or an authenticated peer
/// [`Frame`]. The `from == me` split is resolved in [`Relay::decode`].
pub enum RelayEvent<T> {
    /// Runtime service registration (provenance = self).
    Command(RelayCommand<T>),
    /// A network frame from an authenticated peer.
    Frame {
        /// Authenticated sender.
        from: Did,
        /// The frame.
        frame: Frame,
    },
}

/// The relay's own effect algebra (interpreted by `NativeRelay` / `WtRelay`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayEffect<T> {
    /// Open a local backend session to `target` and relay it (peer opened a session).
    Connect {
        /// Owner-scoped session key.
        key: SessionKey,
        /// Local target to dial.
        target: T,
        /// Stream (TCP) or datagram (UDP).
        kind: TransportKind,
    },
    /// Write peer bytes to a session's local stream.
    Write {
        /// Target session.
        key: SessionKey,
        /// Bytes.
        bytes: Bytes,
    },
    /// Half-close a session's local write side (peer FIN).
    Shutdown {
        /// Target session.
        key: SessionKey,
    },
    /// Close a session (full teardown).
    Close {
        /// Target session.
        key: SessionKey,
    },
    /// Reply a `Frame::Close` to a peer that opened an unknown service. The reply goes out under
    /// the interpreter's own namespace (its [`Scope`]), so the effect carries no namespace of
    /// its own.
    SendClose {
        /// Peer to reply to.
        to: Did,
        /// Session id to close.
        session: SessionId,
        /// Whether *we* opened the session (false: the peer did).
        from_opener: bool,
    },
    /// Bind a pending accepted connection/flow (engine-local `token`) to the session `key`
    /// the pure `step` just minted, then open it to the peer and start relaying. The reply
    /// to [`RelayCommand::Accepted`] — this is how a step-minted id reaches the engine.
    OpenAccepted {
        /// Engine-local handle for the pending connection/flow.
        token: u64,
        /// The session key minted by the pure step.
        key: SessionKey,
        /// The remote service to open.
        service: String,
    },
    /// Drop an accepted local resource when the pure session-id space is exhausted.
    RejectAccepted {
        /// Engine-local handle whose resource must be released.
        token: u64,
    },
}

/// Relay state: the service registry and the set of open (server-side, remote-opened)
/// sessions. The live OS/WebTransport resources are the interpreter's engine table; this is
/// the protocol's view used for owner-rejection and duplicate-`Open` rejection.
#[derive(Clone)]
pub struct RelayState<T> {
    services: Arc<HashMap<String, T>>,
    sessions: HashSet<SessionKey>,
    /// TCP sessions whose peer-to-local direction consumed its affine FIN.
    ///
    /// Invariant: `peer_shutdown` is a subset of `sessions`. The reverse direction remains live
    /// until `Close`, but no later peer `Data` can escape the pure reducer into a backend writer.
    peer_shutdown: HashSet<SessionKey>,
    /// Monotonic allocator for client-side session ids. Lives in the **pure** state so the
    /// core (not the engine) mints session identities — `Event → step → Effect` is the sole
    /// authority for both the session set and its ids.
    next_session: u64,
}

impl<T> Default for RelayState<T> {
    fn default() -> Self {
        Self {
            services: Arc::new(HashMap::new()),
            sessions: HashSet::new(),
            peer_shutdown: HashSet::new(),
            next_session: 0,
        }
    }
}

impl<T> RelayState<T> {
    fn can_admit_session(&self, key: &SessionKey) -> bool {
        self.sessions.len() < MAX_RELAY_SESSIONS
            && self
                .sessions
                .iter()
                .filter(|session| session.peer == key.peer)
                .take(MAX_RELAY_SESSIONS_PER_PEER)
                .count()
                < MAX_RELAY_SESSIONS_PER_PEER
    }

    fn insert_session(&mut self, key: SessionKey) {
        self.peer_shutdown.remove(&key);
        self.sessions.insert(key);
    }

    fn remove_session(&mut self, key: &SessionKey) -> bool {
        self.peer_shutdown.remove(key);
        self.sessions.remove(key)
    }

    fn peer_can_send(&self, key: &SessionKey) -> bool {
        self.sessions.contains(key) && !self.peer_shutdown.contains(key)
    }

    /// Consume the peer-to-local FIN exactly once for a live TCP stream.
    fn shutdown_peer(&mut self, key: &SessionKey, kind: TransportKind) -> bool {
        kind == TransportKind::Tcp
            && self.sessions.contains(key)
            && self.peer_shutdown.insert(key.clone())
    }
}

/// Transport relay protocol (server side), generic over the service target `T`.
#[derive(Clone)]
pub struct Relay<T> {
    namespace: String,
    kind: TransportKind,
    config: HashMap<String, T>,
}

impl<T> Relay<T> {
    /// A TCP relay with a fixed service configuration.
    pub fn tcp(config: HashMap<String, T>) -> Self {
        Self {
            namespace: TCP.to_string(),
            kind: TransportKind::Tcp,
            config,
        }
    }

    /// A UDP relay with a fixed service configuration.
    pub fn udp(config: HashMap<String, T>) -> Self {
        Self {
            namespace: UDP.to_string(),
            kind: TransportKind::Udp,
            config,
        }
    }
}

impl<T> Protocol for Relay<T>
where T: Clone + DeserializeOwned + Serialize + MaybeSend + 'static
{
    type State = RelayState<T>;
    type Event = RelayEvent<T>;
    type Effect = RelayEffect<T>;

    fn namespace(&self) -> &str {
        self.namespace.as_str()
    }

    fn init(&self) -> RelayState<T> {
        RelayState {
            services: Arc::new(self.config.clone()),
            sessions: HashSet::new(),
            peer_shutdown: HashSet::new(),
            next_session: 0,
        }
    }

    fn decode(&self, wire: Wire<'_>) -> Result<RelayEvent<T>, Reject> {
        if wire.from == wire.me {
            let command = bincode::deserialize::<RelayCommand<T>>(wire.payload)
                .map_err(|e| Reject(format!("bad relay command: {e}")))?;
            Ok(RelayEvent::Command(command))
        } else {
            let frame = bincode::deserialize::<Frame>(wire.payload)
                .map_err(|e| Reject(format!("bad relay frame: {e}")))?;
            Ok(RelayEvent::Frame {
                from: wire.from,
                frame,
            })
        }
    }

    fn step(
        &self,
        ctx: Ctx<'_, RelayState<T>>,
        event: RelayEvent<T>,
    ) -> Transition<RelayState<T>, RelayEffect<T>> {
        match event {
            RelayEvent::Command(command) => {
                step_command(self.namespace.as_str(), ctx.state, command)
            }
            RelayEvent::Frame { from, frame } => {
                step_frame(self.kind, self.namespace.as_str(), ctx.state, from, frame)
            }
        }
    }
}

/// Apply a local [`RelayCommand`]. Pure. `Accepted`/`Untrack` are the engine→protocol
/// feedback that make `step` the sole authority over the session set **and its ids**: the
/// core mints the id on `Accepted` (the engine reported only a local token) and forgets the
/// session on `Untrack`.
fn step_command<T: Clone>(
    namespace: &str,
    state: &RelayState<T>,
    command: RelayCommand<T>,
) -> Transition<RelayState<T>, RelayEffect<T>> {
    let mut next = state.clone();
    match command {
        RelayCommand::RegisterService { name, target } => {
            Arc::make_mut(&mut next.services).insert(name, target);
            Transition::pure(next)
        }
        RelayCommand::UnregisterService { name } => {
            Arc::make_mut(&mut next.services).remove(&name);
            Transition::pure(next)
        }
        RelayCommand::Accepted {
            token,
            peer,
            service,
        } => {
            // The core mints the session id (the engine reported only its local token), so
            // id allocation is part of the pure state transition, not a shell decision.
            let session = SessionId(next.next_session);
            let key = SessionKey::new(peer, namespace, session, Initiator::Local);
            if !next.can_admit_session(&key) {
                return Transition::with(next, vec![RelayEffect::RejectAccepted { token }]);
            }
            let Some(next_session) = next.next_session.checked_add(1) else {
                return Transition::with(next, vec![RelayEffect::RejectAccepted { token }]);
            };
            next.next_session = next_session;
            // A locally-accepted tunnel: we are the initiator.
            next.insert_session(key.clone());
            Transition::with(next, vec![RelayEffect::OpenAccepted {
                token,
                key,
                service,
            }])
        }
        RelayCommand::Untrack {
            peer,
            session,
            initiator,
        } => {
            next.remove_session(&SessionKey::new(peer, namespace, session, initiator));
            Transition::pure(next)
        }
        RelayCommand::Abort {
            peer,
            session,
            initiator,
        } => {
            let key = SessionKey::new(peer, namespace, session, initiator);
            if next.remove_session(&key) {
                Transition::with(next, vec![RelayEffect::SendClose {
                    to: peer,
                    session,
                    from_opener: matches!(initiator, Initiator::Local),
                }])
            } else {
                Transition::pure(next)
            }
        }
    }
}

/// Apply a network [`Frame`]. Pure; emits relay effects scoped to the authenticated `from`.
fn step_frame<T: Clone>(
    kind: TransportKind,
    namespace: &str,
    state: &RelayState<T>,
    from: Did,
    frame: Frame,
) -> Transition<RelayState<T>, RelayEffect<T>> {
    match frame {
        // `Open` is always sent by the opener, so from our side the peer is the initiator.
        Frame::Open { session, service } => {
            let key = SessionKey::new(from, namespace, session, Initiator::Remote);
            // Reject a duplicate/retried Open for a session this peer already holds open.
            if state.sessions.contains(&key) {
                return Transition::pure(state.clone());
            }
            match state.services.get(service.as_str()) {
                Some(target) => {
                    let mut next = state.clone();
                    if !next.can_admit_session(&key) {
                        return rejected_open(next, from, session);
                    }
                    let target = target.clone();
                    next.insert_session(key.clone());
                    Transition::with(next, vec![RelayEffect::Connect { key, target, kind }])
                }
                None => rejected_open(state.clone(), from, session),
            }
        }
        // Data/Shutdown/Close are guarded on the authoritative session set: the *reducer*
        // decides whether the effect happens, not the engine table. `from_opener` (the
        // sender opened it) flips to our initiator.
        Frame::Data {
            session,
            from_opener,
            bytes,
        } => {
            let key = SessionKey::new(from, namespace, session, opener_to_initiator(from_opener));
            if state.peer_can_send(&key) {
                Transition::with(state.clone(), vec![RelayEffect::Write { key, bytes }])
            } else {
                Transition::pure(state.clone())
            }
        }
        Frame::Shutdown {
            session,
            from_opener,
        } => {
            let key = SessionKey::new(from, namespace, session, opener_to_initiator(from_opener));
            let mut next = state.clone();
            if next.shutdown_peer(&key, kind) {
                Transition::with(next, vec![RelayEffect::Shutdown { key }])
            } else {
                Transition::pure(next)
            }
        }
        Frame::Close {
            session,
            from_opener,
        } => {
            let key = SessionKey::new(from, namespace, session, opener_to_initiator(from_opener));
            if state.sessions.contains(&key) {
                let mut next = state.clone();
                next.remove_session(&key);
                Transition::with(next, vec![RelayEffect::Close { key }])
            } else {
                Transition::pure(state.clone())
            }
        }
    }
}

/// Reject a peer-opened session without allocating reducer or interpreter state.
fn rejected_open<T>(
    state: RelayState<T>,
    peer: Did,
    session: SessionId,
) -> Transition<RelayState<T>, RelayEffect<T>> {
    Transition::with(state, vec![RelayEffect::SendClose {
        to: peer,
        session,
        // The peer opened it; this endpoint did not.
        from_opener: false,
    }])
}

/// Map a frame's `from_opener` (the **sender** opened the session) to our own [`Initiator`].
fn opener_to_initiator(from_opener: bool) -> Initiator {
    if from_opener {
        Initiator::Remote
    } else {
        Initiator::Local
    }
}

/// Encode a `Frame::Close` as bytes for an overlay send. `from_opener` is whether *we* (the
/// sender of this close) opened the session.
pub(crate) fn close_frame(session: SessionId, from_opener: bool) -> Bytes {
    let frame = Frame::Close {
        session,
        from_opener,
    };
    Bytes::from(bincode::serialize(&frame).unwrap_or_default())
}

// ── Native interpreter (OS sockets) ───────────────────────────────────────────────────

/// Native relay interpreter: runs [`RelayEffect`]s over the OS-socket engine it owns. The
/// engine uses the namespace-scoped [`Scope`] capability for both overlay sends and lifecycle
/// feedback (`Accepted`/`Untrack`), so the engine has no `Processor` of its own.
#[cfg(feature = "node")]
pub(crate) struct NativeRelay {
    engine: Arc<crate::extension::transport::engine::TransportSessions>,
}

#[cfg(feature = "node")]
impl NativeRelay {
    /// Build over a shared engine.
    pub(crate) fn new(engine: Arc<crate::extension::transport::engine::TransportSessions>) -> Self {
        Self { engine }
    }
}

#[cfg(feature = "node")]
#[async_trait::async_trait]
impl Interpret for NativeRelay {
    type Effect = RelayEffect<std::net::SocketAddr>;

    async fn run(
        &self,
        scope: &EffectScope,
        effect: RelayEffect<std::net::SocketAddr>,
    ) -> crate::error::Result<Vec<Bytes>> {
        match effect {
            RelayEffect::Connect { key, target, kind } => {
                let admission = self
                    .engine
                    .clone()
                    .connect(scope.lifecycle(), key.clone(), target, kind)
                    .await;
                return enqueue_feedback::<std::net::SocketAddr>(key, admission);
            }
            RelayEffect::Write { key, bytes } => {
                let admission = self.engine.write(&key, bytes);
                return enqueue_feedback::<std::net::SocketAddr>(key, admission);
            }
            RelayEffect::Shutdown { key } => {
                let admission = self.engine.shutdown(&key);
                return enqueue_feedback::<std::net::SocketAddr>(key, admission);
            }
            RelayEffect::Close { key } => {
                self.engine.close_for_effect(&key);
            }
            RelayEffect::SendClose {
                to,
                session,
                from_opener,
            } => {
                scope.send(to, close_frame(session, from_opener)).await?;
            }
            RelayEffect::OpenAccepted {
                token,
                key,
                service,
            } => {
                let feedback = self
                    .engine
                    .clone()
                    .bind_accepted(scope.lifecycle(), token, key, service)
                    .await;
                return feedback
                    .map(untrack_feedback::<std::net::SocketAddr>)
                    .transpose()
                    .map(|feedback| feedback.into_iter().collect());
            }
            RelayEffect::RejectAccepted { token } => {
                self.engine.evict_pending_for_effect(token);
            }
        }
        Ok(Vec::new())
    }
}

/// Encode an engine teardown as the relay's synchronous, ordered feedback path.
fn untrack_feedback<T: Serialize>(key: SessionKey) -> crate::error::Result<Bytes> {
    let command = RelayCommand::<T>::Untrack {
        peer: key.peer,
        session: key.session,
        initiator: key.initiator,
    };
    bincode::serialize(&command)
        .map(Bytes::from)
        .map_err(|_| crate::error::Error::EncodeError)
}

/// Encode a synchronous engine failure so the pure reducer owns both state
/// removal and the exactly-once peer-facing terminal effect.
fn abort_feedback<T: Serialize>(key: SessionKey) -> crate::error::Result<Bytes> {
    let command = RelayCommand::<T>::Abort {
        peer: key.peer,
        session: key.session,
        initiator: key.initiator,
    };
    bincode::serialize(&command)
        .map(Bytes::from)
        .map_err(|_| crate::error::Error::EncodeError)
}

/// Map backend admission into the relay reducer's ordered feedback algebra.
fn enqueue_feedback<T: Serialize>(
    key: SessionKey,
    admission: EffectEnqueue,
) -> crate::error::Result<Vec<Bytes>> {
    match admission {
        EffectEnqueue::Enqueued => Ok(Vec::new()),
        EffectEnqueue::Missing => untrack_feedback::<T>(key).map(|feedback| vec![feedback]),
        EffectEnqueue::Failed => abort_feedback::<T>(key).map(|feedback| vec![feedback]),
    }
}

// ── Browser interpreter (WebTransport) ────────────────────────────────────────────────

/// Browser relay interpreter: runs [`RelayEffect`]s over the WebTransport engine it owns.
#[cfg(all(feature = "browser", target_family = "wasm"))]
pub(crate) struct WtRelay {
    engine: Arc<crate::extension::transport::wt::WtSessions>,
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
impl WtRelay {
    /// Build over a shared WebTransport engine.
    pub(crate) fn new(engine: Arc<crate::extension::transport::wt::WtSessions>) -> Self {
        Self { engine }
    }
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
#[async_trait::async_trait(?Send)]
impl Interpret for WtRelay {
    type Effect = RelayEffect<String>;

    async fn run(
        &self,
        scope: &EffectScope,
        effect: RelayEffect<String>,
    ) -> crate::error::Result<Vec<Bytes>> {
        match effect {
            RelayEffect::Connect { key, target, kind } => {
                let admission =
                    self.engine
                        .clone()
                        .connect(scope.lifecycle(), key.clone(), target, kind);
                return enqueue_feedback::<String>(key, admission);
            }
            RelayEffect::Write { key, bytes } => {
                let admission = self.engine.write(scope.lifecycle(), key.clone(), bytes);
                return enqueue_feedback::<String>(key, admission);
            }
            RelayEffect::Shutdown { key } => {
                let admission = self.engine.shutdown(scope.lifecycle(), key.clone());
                return enqueue_feedback::<String>(key, admission);
            }
            RelayEffect::Close { key } => {
                self.engine.close_for_effect(&key);
            }
            RelayEffect::SendClose {
                to,
                session,
                from_opener,
            } => {
                scope.send(to, close_frame(session, from_opener)).await?;
            }
            // The browser relay is server-side only (no local listener), so it never reports
            // an `Accepted` and thus never receives `OpenAccepted`.
            RelayEffect::OpenAccepted { .. } => {
                tracing::warn!("browser relay received OpenAccepted; it has no local listener");
            }
            RelayEffect::RejectAccepted { .. } => {
                tracing::warn!("browser relay received RejectAccepted; it has no local listener");
            }
        }
        Ok(Vec::new())
    }
}

// ── Client-side relay handle ──────────────────────────────────────────────────────────

/// Client-facing handle to the relay extension's live engine: open local tunnels and register
/// local services. This is the relay extension's *own* surface — the relay owns its engine and
/// installs itself ([`install`](RelayHandle::install)), so nothing about it leaks into the
/// generic [`Provider`](crate::provider::Provider) (the same way SNARK registers itself).
/// Cloneable; every clone drives the same shared engine and pure [`Relay`] state.
/// Holds the two per-namespace scoped capabilities (`tcp` / `udp`); each method picks one and
/// can only act within it, so the handle cannot address an arbitrary namespace even internally.
#[cfg(feature = "node")]
#[derive(Clone)]
pub struct RelayHandle {
    engine: Arc<crate::extension::transport::engine::TransportSessions>,
    tcp: Scope,
    udp: Scope,
}

#[cfg(feature = "node")]
impl RelayHandle {
    /// Install the relay into an extension registry: register the TCP and UDP interpreters
    /// over a fresh, relay-owned OS-socket engine and return the client handle. Errors if the
    /// `tcp`/`udp` namespaces are already taken. Call once per node, after constructing the
    /// provider — the relay is opt-in, not a `Provider` invariant.
    pub fn install(extensions: &crate::extension::ext::Extensions) -> crate::error::Result<Self> {
        let engine = Arc::new(crate::extension::transport::engine::TransportSessions::new());
        // Atomic: both namespaces register together, or neither (no half-installed relay).
        extensions.register_many(vec![
            (Relay::tcp(HashMap::new()), NativeRelay::new(engine.clone())),
            (Relay::udp(HashMap::new()), NativeRelay::new(engine.clone())),
        ])?;
        let core = extensions.core();
        Ok(Self {
            engine,
            tcp: Scope::new(core.clone(), TCP.to_string()),
            udp: Scope::new(core, UDP.to_string()),
        })
    }

    /// Open a local **TCP** tunnel: bind `local_addr` and relay each accepted connection to
    /// `peer`'s `service` (client side, forward proxy).
    pub async fn open_tcp_tunnel(
        &self,
        local_addr: std::net::SocketAddr,
        peer: Did,
        service: String,
    ) -> crate::error::Result<()> {
        self.open_tunnel(&self.tcp, local_addr, peer, service, TransportKind::Tcp)
            .await
    }

    /// Relay one already-accepted **TCP** stream to `peer`'s `service`.
    pub async fn relay_tcp_stream(
        &self,
        stream: tokio::net::TcpStream,
        peer: Did,
        service: String,
    ) -> crate::error::Result<()> {
        self.engine
            .clone()
            .relay_tcp_stream(self.tcp.clone(), stream, peer, service)
            .await;
        Ok(())
    }

    /// Open a local **UDP** tunnel: bind `local_addr` and relay each datagram flow to `peer`'s
    /// `service` (client side, forward proxy).
    pub async fn open_udp_tunnel(
        &self,
        local_addr: std::net::SocketAddr,
        peer: Did,
        service: String,
    ) -> crate::error::Result<()> {
        self.open_tunnel(&self.udp, local_addr, peer, service, TransportKind::Udp)
            .await
    }

    async fn open_tunnel(
        &self,
        scope: &Scope,
        local_addr: std::net::SocketAddr,
        peer: Did,
        service: String,
        kind: TransportKind,
    ) -> crate::error::Result<()> {
        // Bind a local listener on the relay engine with this namespace's scope. Each accepted
        // connection is reported back through the pure relay (`Accepted`), so
        // `RelayState.sessions` stays the sole authority.
        self.engine
            .clone()
            .listen(scope.clone(), local_addr, peer, service, kind)
            .await;
        Ok(())
    }

    /// Register (at runtime) a local service the **TCP** relay may dial (`name` → `addr`).
    pub async fn register_tcp_service(
        &self,
        name: String,
        addr: std::net::SocketAddr,
    ) -> crate::error::Result<()> {
        register_service(&self.tcp, name, addr).await
    }

    /// Register (at runtime) a local service the **UDP** relay may dial (`name` → `addr`).
    pub async fn register_udp_service(
        &self,
        name: String,
        addr: std::net::SocketAddr,
    ) -> crate::error::Result<()> {
        register_service(&self.udp, name, addr).await
    }
}

/// Map a service `name` → `target` by self-injecting a `RegisterService` command into the
/// scope's own namespace (provenance = self).
#[cfg(any(feature = "node", all(feature = "browser", target_family = "wasm")))]
async fn register_service<T>(scope: &Scope, name: String, target: T) -> crate::error::Result<()>
where T: Serialize {
    let command = RelayCommand::RegisterService { name, target };
    let payload = bincode::serialize(&command).map_err(|_| crate::error::Error::EncodeError)?;
    scope.inject(Bytes::from(payload)).await
}

/// Client-facing handle to the browser relay extension's live WebTransport engine: register
/// local WebTransport-backed services. The browser relay is server-side only (no local
/// listener), so it has no tunnel-open surface. Cloneable. See the native [`RelayHandle`].
/// Holds the two per-namespace scoped capabilities (`tcp` / `udp`). The browser relay is
/// server-side only (no local listener), so this handle just registers services. See the
/// native [`RelayHandle`].
#[cfg(all(feature = "browser", target_family = "wasm"))]
#[derive(Clone)]
pub struct RelayHandle {
    tcp: Scope,
    udp: Scope,
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
impl RelayHandle {
    /// Install the browser relay into an extension registry: register the TCP and UDP
    /// interpreters over a fresh, relay-owned WebTransport engine and return the client handle.
    /// Errors if the `tcp`/`udp` namespaces are already taken. Call once per node, after
    /// constructing the provider — the relay is opt-in, not a `Provider` invariant.
    ///
    /// This is a **Rust-wasm-facing** surface: there is no `wasm_bindgen` install/handle for JS
    /// yet (unlike `provider.on(...)`), so browser relay is reachable only from Rust-wasm apps.
    /// A JS-facing extension install API can be added when a JS consumer needs WebTransport
    /// relay; it must not put these methods back on the generic `Provider`.
    pub fn install(extensions: &crate::extension::ext::Extensions) -> crate::error::Result<Self> {
        let engine = Arc::new(crate::extension::transport::wt::WtSessions::new());
        // Atomic: both namespaces register together, or neither (no half-installed relay).
        extensions.register_many(vec![
            (Relay::tcp(HashMap::new()), WtRelay::new(engine.clone())),
            (Relay::udp(HashMap::new()), WtRelay::new(engine)),
        ])?;
        let core = extensions.core();
        Ok(Self {
            tcp: Scope::new(core.clone(), TCP.to_string()),
            udp: Scope::new(core, UDP.to_string()),
        })
    }

    /// Register a WebTransport-backed service for the browser **TCP** relay, mapping
    /// `name` → WebTransport `url` (under the `tcp` namespace).
    pub async fn register_wt_service(&self, name: String, url: String) -> crate::error::Result<()> {
        register_service(&self.tcp, name, url).await
    }

    /// Register a WebTransport-backed service for the browser **UDP** relay (datagrams),
    /// mapping `name` → WebTransport `url` (under the `udp` namespace).
    pub async fn register_wt_udp_service(
        &self,
        name: String,
        url: String,
    ) -> crate::error::Result<()> {
        register_service(&self.udp, name, url).await
    }
}

#[cfg(test)]
mod tests;
