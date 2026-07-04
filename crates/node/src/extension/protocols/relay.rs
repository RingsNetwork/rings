#![warn(missing_docs)]
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
//!   step (Frame(from, Data s b)) | k∈sessions   ↦ (S, [Write k b])   else (S, ε)
//!   step (Frame(from, Close s))  | k∈sessions   ↦ (S∖{k}, [Close k]) else (S, ε)
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
use crate::extension::ext::Interpret;
use crate::extension::ext::MaybeSend;
use crate::extension::ext::Protocol;
use crate::extension::ext::Reject;
use crate::extension::ext::Scope;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;
use crate::extension::transport::Frame;
use crate::extension::transport::Initiator;
use crate::extension::transport::SessionId;
use crate::extension::transport::SessionKey;
use crate::extension::transport::TransportKind;

/// Namespace for the TCP relay.
pub const TCP: &str = "tcp";
/// Namespace for the UDP relay.
pub const UDP: &str = "udp";

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
}

/// Relay state: the service registry and the set of open (server-side, remote-opened)
/// sessions. The live OS/WebTransport resources are the interpreter's engine table; this is
/// the protocol's view used for owner-rejection and duplicate-`Open` rejection.
#[derive(Clone)]
pub struct RelayState<T> {
    services: Arc<HashMap<String, T>>,
    sessions: HashSet<SessionKey>,
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
            next_session: 0,
        }
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
            next.next_session += 1;
            // A locally-accepted tunnel: we are the initiator.
            let key = SessionKey::new(peer, namespace, session, Initiator::Local);
            next.sessions.insert(key.clone());
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
            next.sessions
                .remove(&SessionKey::new(peer, namespace, session, initiator));
            Transition::pure(next)
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
                    let target = target.clone();
                    let mut next = state.clone();
                    next.sessions.insert(key.clone());
                    Transition::with(next, vec![RelayEffect::Connect { key, target, kind }])
                }
                None => Transition::with(state.clone(), vec![RelayEffect::SendClose {
                    to: from,
                    session,
                    // The peer opened it (unknown service); we did not.
                    from_opener: false,
                }]),
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
            if state.sessions.contains(&key) {
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
            if state.sessions.contains(&key) {
                Transition::with(state.clone(), vec![RelayEffect::Shutdown { key }])
            } else {
                Transition::pure(state.clone())
            }
        }
        Frame::Close {
            session,
            from_opener,
        } => {
            let key = SessionKey::new(from, namespace, session, opener_to_initiator(from_opener));
            if state.sessions.contains(&key) {
                let mut next = state.clone();
                next.sessions.remove(&key);
                Transition::with(next, vec![RelayEffect::Close { key }])
            } else {
                Transition::pure(state.clone())
            }
        }
    }
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
        scope: &Scope,
        effect: RelayEffect<std::net::SocketAddr>,
    ) -> crate::error::Result<Vec<Bytes>> {
        match effect {
            RelayEffect::Connect { key, target, kind } => {
                self.engine
                    .clone()
                    .connect(scope.clone(), key, target, kind)
                    .await;
            }
            RelayEffect::Write { key, bytes } => {
                self.engine.write(&key, bytes).await;
            }
            RelayEffect::Shutdown { key } => {
                self.engine.shutdown(&key).await;
            }
            RelayEffect::Close { key } => {
                self.engine.close(scope, &key).await;
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
                self.engine
                    .clone()
                    .bind_accepted(scope.clone(), token, key, service)
                    .await;
            }
        }
        Ok(Vec::new())
    }
}

// ── Browser interpreter (WebTransport) ────────────────────────────────────────────────

/// Browser relay interpreter: runs [`RelayEffect`]s over the WebTransport engine it owns.
#[cfg(feature = "browser")]
pub(crate) struct WtRelay {
    engine: Arc<crate::extension::transport::wt::WtSessions>,
}

#[cfg(feature = "browser")]
impl WtRelay {
    /// Build over a shared WebTransport engine.
    pub(crate) fn new(engine: Arc<crate::extension::transport::wt::WtSessions>) -> Self {
        Self { engine }
    }
}

#[cfg(feature = "browser")]
#[async_trait::async_trait(?Send)]
impl Interpret for WtRelay {
    type Effect = RelayEffect<String>;

    async fn run(
        &self,
        scope: &Scope,
        effect: RelayEffect<String>,
    ) -> crate::error::Result<Vec<Bytes>> {
        match effect {
            RelayEffect::Connect { key, target, kind } => {
                self.engine
                    .clone()
                    .connect(scope.clone(), key, target, kind)
                    .await;
            }
            RelayEffect::Write { key, bytes } => {
                self.engine.write(&key, bytes).await;
            }
            RelayEffect::Shutdown { key } => {
                self.engine.shutdown(&key).await;
            }
            RelayEffect::Close { key } => {
                self.engine.close(scope, &key).await;
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
#[cfg(any(feature = "node", feature = "browser"))]
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
#[cfg(feature = "browser")]
#[derive(Clone)]
pub struct RelayHandle {
    tcp: Scope,
    udp: Scope,
}

#[cfg(feature = "browser")]
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
mod tests {
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::net::SocketAddr;

    use bytes::Bytes;
    use rings_core::dht::Did;

    use super::Frame;
    use super::Initiator;
    use super::Relay;
    use super::RelayCommand;
    use super::RelayEffect;
    use super::RelayState;
    use super::SessionId;
    use super::SessionKey;
    use super::TransportKind;
    use crate::extension::ext::Ctx;
    use crate::extension::ext::Protocol;
    use crate::extension::ext::Transition;
    use crate::extension::ext::Wire;

    fn this_node() -> Did {
        Did::from(1u32)
    }
    fn peer_a() -> Did {
        Did::from(2u32)
    }
    fn peer_b() -> Did {
        Did::from(3u32)
    }
    fn web_addr() -> SocketAddr {
        "127.0.0.1:8080".parse().unwrap()
    }

    /// A server-side (peer-opened) key on the TCP relay — the common case in these tests.
    fn rkey(peer: Did, session: u64) -> SessionKey {
        SessionKey::new(peer, super::TCP, SessionId(session), Initiator::Remote)
    }
    /// Peer `Data` on a peer-opened session (`from_opener = true`).
    fn data(session: u64, bytes: &'static [u8]) -> Frame {
        Frame::Data {
            session: SessionId(session),
            from_opener: true,
            bytes: Bytes::from_static(bytes),
        }
    }
    /// Peer `Close` on a peer-opened session (`from_opener = true`).
    fn close(session: u64) -> Frame {
        Frame::Close {
            session: SessionId(session),
            from_opener: true,
        }
    }
    /// Peer `Open` for `service`.
    fn open(session: u64, service: &str) -> Frame {
        Frame::Open {
            session: SessionId(session),
            service: service.to_string(),
        }
    }

    fn web_relay() -> Relay<SocketAddr> {
        let mut config = HashMap::new();
        config.insert("web".to_string(), web_addr());
        Relay::tcp(config)
    }

    /// Decode a peer frame then step.
    fn step_frame(
        relay: &Relay<SocketAddr>,
        state: &RelayState<SocketAddr>,
        from: Did,
        frame: &Frame,
    ) -> Transition<RelayState<SocketAddr>, RelayEffect<SocketAddr>> {
        let payload = bincode::serialize(frame).unwrap();
        let event = relay
            .decode(Wire {
                from,
                me: this_node(),
                payload: payload.as_ref(),
            })
            .unwrap();
        relay.step(
            Ctx {
                did: this_node(),
                state,
            },
            event,
        )
    }

    /// Decode a self command then step.
    fn step_command(
        relay: &Relay<SocketAddr>,
        state: &RelayState<SocketAddr>,
        command: &RelayCommand<SocketAddr>,
    ) -> Transition<RelayState<SocketAddr>, RelayEffect<SocketAddr>> {
        let payload = bincode::serialize(command).unwrap();
        let event = relay
            .decode(Wire {
                from: this_node(),
                me: this_node(),
                payload: payload.as_ref(),
            })
            .unwrap();
        relay.step(
            Ctx {
                did: this_node(),
                state,
            },
            event,
        )
    }

    #[test]
    fn open_known_service_connects_and_records_the_session() {
        let relay = web_relay();
        let t = step_frame(&relay, &relay.init(), peer_a(), &open(7, "web"));
        let expected = rkey(peer_a(), 7);
        match t.effects.as_slice() {
            [RelayEffect::Connect { key, target, kind }] => {
                assert_eq!(*key, expected);
                assert_eq!(*target, web_addr());
                assert!(matches!(kind, TransportKind::Tcp));
            }
            other => panic!("expected one Connect, got {other:?}"),
        }
        assert!(t.state.sessions.contains(&expected));
    }

    #[test]
    fn duplicate_open_for_a_live_session_is_rejected() {
        let relay = web_relay();
        let opened = step_frame(&relay, &relay.init(), peer_a(), &open(7, "web"));
        assert!(opened.state.sessions.contains(&rkey(peer_a(), 7)));
        let again = step_frame(&relay, &opened.state, peer_a(), &open(7, "web"));
        assert!(
            again.effects.is_empty(),
            "duplicate Open must emit no effect"
        );
        assert_eq!(again.state.sessions.len(), 1);
    }

    #[test]
    fn open_unknown_service_closes_and_records_nothing() {
        let relay = web_relay();
        let t = step_frame(&relay, &relay.init(), peer_a(), &open(7, "ssh"));
        match t.effects.as_slice() {
            [RelayEffect::SendClose {
                to,
                session,
                from_opener,
            }] => {
                assert_eq!(*to, peer_a());
                assert_eq!(*session, SessionId(7));
                assert!(!from_opener, "we are not the opener of the peer's session");
            }
            other => panic!("expected one SendClose, got {other:?}"),
        }
        assert!(t.state.sessions.is_empty());
    }

    #[test]
    fn data_writes_to_a_live_keyed_session() {
        let relay = web_relay();
        // Data is now guarded on the session set, so open it first.
        let opened = step_frame(&relay, &relay.init(), peer_a(), &open(7, "web"));
        let t = step_frame(&relay, &opened.state, peer_a(), &data(7, b"hello"));
        match t.effects.as_slice() {
            [RelayEffect::Write { key, bytes }] => {
                assert_eq!(*key, rkey(peer_a(), 7));
                assert_eq!(bytes.as_ref(), b"hello");
            }
            other => panic!("expected one Write, got {other:?}"),
        }
    }

    #[test]
    fn data_for_an_unknown_session_is_dropped_by_the_reducer() {
        let relay = web_relay();
        // No Open first: the reducer (not the engine table) decides there is no such session.
        let t = step_frame(&relay, &relay.init(), peer_a(), &data(7, b"hello"));
        assert!(
            t.effects.is_empty(),
            "Data for an unknown session emits nothing"
        );
    }

    #[test]
    fn close_removes_the_session_and_emits_close() {
        let relay = web_relay();
        let opened = step_frame(&relay, &relay.init(), peer_a(), &open(7, "web"));
        let t = step_frame(&relay, &opened.state, peer_a(), &close(7));
        let expected = rkey(peer_a(), 7);
        match t.effects.as_slice() {
            [RelayEffect::Close { key }] => assert_eq!(*key, expected),
            other => panic!("expected one Close, got {other:?}"),
        }
        assert!(!t.state.sessions.contains(&expected));
    }

    #[test]
    fn register_service_via_self_command_then_open_connects() {
        let relay = Relay::tcp(HashMap::new());
        let registered = step_command(&relay, &relay.init(), &RelayCommand::RegisterService {
            name: "web".to_string(),
            target: web_addr(),
        });
        assert!(registered.effects.is_empty());
        let t = step_frame(&relay, &registered.state, peer_a(), &open(1, "web"));
        match t.effects.as_slice() {
            [RelayEffect::Connect { target, .. }] => assert_eq!(*target, web_addr()),
            other => panic!("expected one Connect, got {other:?}"),
        }
    }

    #[test]
    fn accepted_mints_in_the_core_then_untrack_removes() {
        let relay = web_relay();
        // A client-side accept is fed back as `Accepted{token}`. The core mints the session id
        // (0 on fresh state, initiator Local) and replies OpenAccepted with that minted key.
        let accepted = step_command(&relay, &relay.init(), &RelayCommand::Accepted {
            token: 42,
            peer: peer_a(),
            service: "web".to_string(),
        });
        let key = SessionKey::new(peer_a(), super::TCP, SessionId(0), Initiator::Local);
        match accepted.effects.as_slice() {
            [RelayEffect::OpenAccepted {
                token,
                key: k,
                service,
            }] => {
                assert_eq!(*token, 42);
                assert_eq!(*k, key);
                assert_eq!(service, "web");
            }
            other => panic!("expected one OpenAccepted, got {other:?}"),
        }
        assert!(accepted.state.sessions.contains(&key));

        let untracked = step_command(&relay, &accepted.state, &RelayCommand::Untrack {
            peer: peer_a(),
            session: SessionId(0),
            initiator: Initiator::Local,
        });
        assert!(untracked.effects.is_empty());
        assert!(!untracked.state.sessions.contains(&key));
    }

    #[test]
    fn a_peer_cannot_address_another_peers_session() {
        let relay = web_relay();
        let a_open = step_frame(&relay, &relay.init(), peer_a(), &open(0, "web"));
        let key_a = rkey(peer_a(), 0);
        assert!(a_open.state.sessions.contains(&key_a));

        // peer B references session 0 (same id) but never opened it here: the reducer drops
        // both Data and Close — A's session is untouched. (Owner rejection in the core.)
        let b_data = step_frame(&relay, &a_open.state, peer_b(), &data(0, b"x"));
        assert!(
            b_data.effects.is_empty(),
            "B's Data for a session it did not open is dropped"
        );
        let b_close = step_frame(&relay, &a_open.state, peer_b(), &close(0));
        assert!(
            b_close.effects.is_empty(),
            "B's Close for a session it did not open is dropped"
        );
        assert!(b_close.state.sessions.contains(&key_a));
    }

    #[test]
    fn local_and_remote_sessions_with_the_same_id_do_not_collide() {
        // Bidirectional open against the same peer, both id 0: a peer-opened (Remote) session
        // and a locally-accepted (Local) session must be distinct keys.
        let relay = web_relay();
        let opened = step_frame(&relay, &relay.init(), peer_a(), &open(0, "web"));
        let accepted = step_command(&relay, &opened.state, &RelayCommand::Accepted {
            token: 1,
            peer: peer_a(),
            service: "web".to_string(),
        });
        let remote = SessionKey::new(peer_a(), super::TCP, SessionId(0), Initiator::Remote);
        let local = SessionKey::new(peer_a(), super::TCP, SessionId(0), Initiator::Local);
        assert_ne!(remote, local);
        assert!(accepted.state.sessions.contains(&remote));
        assert!(accepted.state.sessions.contains(&local));
        assert_eq!(accepted.state.sessions.len(), 2);
    }

    // ── lifecycle property tests (reviewer-requested) ─────────────────────────────────

    #[test]
    fn open_then_close_then_data_does_not_resurrect_the_session() {
        let relay = web_relay();
        let key = rkey(peer_a(), 3);
        let opened = step_frame(&relay, &relay.init(), peer_a(), &open(3, "web"));
        assert!(opened.state.sessions.contains(&key));
        let closed = step_frame(&relay, &opened.state, peer_a(), &close(3));
        assert!(!closed.state.sessions.contains(&key));
        // A late Data for the now-closed session is dropped by the reducer (guarded on the
        // session set) — no effect, no resurrection.
        let late = step_frame(&relay, &closed.state, peer_a(), &data(3, b"late"));
        assert!(late.effects.is_empty());
        assert!(late.state.sessions.is_empty());
    }

    #[test]
    fn close_after_close_is_idempotent() {
        let relay = web_relay();
        let opened = step_frame(&relay, &relay.init(), peer_a(), &open(5, "web"));
        let c1 = step_frame(&relay, &opened.state, peer_a(), &close(5));
        assert!(matches!(c1.effects.as_slice(), [RelayEffect::Close { .. }]));
        // The second close hits no live session: the reducer drops it (no effect, no panic).
        let c2 = step_frame(&relay, &c1.state, peer_a(), &close(5));
        assert!(c2.effects.is_empty());
        assert!(c2.state.sessions.is_empty());
    }

    #[test]
    fn malformed_payload_is_rejected_at_the_boundary() {
        let relay = web_relay();
        let bad = [0xFFu8, 0xFF, 0xFF, 0xFF, 0xFF];
        let result = relay.decode(Wire {
            from: peer_a(),
            me: this_node(),
            payload: &bad,
        });
        assert!(result.is_err(), "a malformed frame must be rejected");
    }

    /// Property: across a long, deterministic, collision-prone interleaving of peer frames
    /// (Open/Data/Close) from several peers, the pure `State.sessions` never diverges from an
    /// independent model, and Data/Close only ever act on live sessions (reducer authority).
    #[test]
    fn lifecycle_property_state_never_diverges_from_model() {
        let relay = web_relay();
        let peers = [peer_a(), peer_b(), Did::from(4u32)];
        let mut state = relay.init();
        let mut model: HashSet<SessionKey> = HashSet::new();
        let mut rng: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        for _ in 0..4000 {
            let r = next();
            let peer = peers[(r % 3) as usize];
            let session = (r >> 2) & 0x7; // 8 ids → frequent collisions
            let key = rkey(peer, session);
            let transition = match (r >> 8) % 4 {
                0 => {
                    let t = step_frame(&relay, &state, peer, &open(session, "web"));
                    if model.contains(&key) {
                        assert!(t.effects.is_empty(), "duplicate Open must emit nothing");
                    } else {
                        assert!(matches!(t.effects.as_slice(), [
                            RelayEffect::Connect { .. }
                        ]));
                        model.insert(key.clone());
                    }
                    t
                }
                1 => {
                    let t = step_frame(&relay, &state, peer, &open(session, "nope"));
                    if model.contains(&key) {
                        assert!(t.effects.is_empty());
                    } else {
                        assert!(matches!(t.effects.as_slice(), [
                            RelayEffect::SendClose { .. }
                        ]));
                    }
                    t
                }
                2 => {
                    let t = step_frame(&relay, &state, peer, &data(session, b"x"));
                    if model.contains(&key) {
                        match t.effects.as_slice() {
                            [RelayEffect::Write { key: k, .. }] => assert_eq!(*k, key),
                            other => panic!("expected one Write, got {other:?}"),
                        }
                    } else {
                        assert!(
                            t.effects.is_empty(),
                            "Data on an unknown session is dropped"
                        );
                    }
                    t
                }
                _ => {
                    let t = step_frame(&relay, &state, peer, &close(session));
                    if model.contains(&key) {
                        assert!(matches!(t.effects.as_slice(), [RelayEffect::Close { .. }]));
                    } else {
                        assert!(
                            t.effects.is_empty(),
                            "Close on an unknown session is dropped"
                        );
                    }
                    model.remove(&key);
                    t
                }
            };
            state = transition.state;
            assert_eq!(
                state.sessions, model,
                "State.sessions diverged from the model"
            );
        }
    }

    /// A faithful in-test model of a relay engine's resource table: `key → generation`,
    /// mirroring `register` (insert a fresh generation), `close` (drop the current handle) and
    /// `close_if_current` (drop only if the generation matches, returning whether it did).
    /// This logic is identical in the native [`TransportSessions`] and browser `WtSessions`
    /// engines, so the model covers both — only the socket vs. WebTransport plumbing differs.
    struct EngineModel {
        map: HashMap<SessionKey, u64>,
        next_gen: u64,
    }

    impl EngineModel {
        fn new() -> Self {
            Self {
                map: HashMap::new(),
                next_gen: 0,
            }
        }
        fn register(&mut self, key: SessionKey) -> u64 {
            let gen = self.next_gen;
            self.next_gen += 1;
            self.map.insert(key, gen);
            gen
        }
        fn close(&mut self, key: &SessionKey) {
            self.map.remove(key);
        }
        /// Returns whether it was the current owner (and removed it). A `false` here means the
        /// caller is a stale task: it must send the peer **no** `Close` either.
        fn close_if_current(&mut self, key: &SessionKey, gen: u64) -> bool {
            if self.map.get(key) == Some(&gen) {
                self.map.remove(key);
                true
            } else {
                false
            }
        }
    }

    /// Apply a step's effects to the engine model, returning the generation of any handle the
    /// effects registered (a relay task's captured generation). Asserts `Write`/`Shutdown`
    /// only ever hit a live handle — i.e. the reducer, not the engine table, decided.
    fn apply_effects(
        eng: &mut EngineModel,
        effects: &[RelayEffect<SocketAddr>],
    ) -> Option<(SessionKey, u64)> {
        let mut registered = None;
        for effect in effects {
            match effect {
                RelayEffect::Connect { key, .. } | RelayEffect::OpenAccepted { key, .. } => {
                    registered = Some((key.clone(), eng.register(key.clone())));
                }
                RelayEffect::Write { key, .. } | RelayEffect::Shutdown { key } => {
                    assert!(
                        eng.map.contains_key(key),
                        "effect targeted a non-live session"
                    );
                }
                RelayEffect::Close { key } => eng.close(key),
                RelayEffect::SendClose { .. } => {}
            }
        }
        registered
    }

    #[test]
    fn generation_prevents_a_slow_old_task_deleting_a_reopened_handle() {
        // Server session id 7 is opener-chosen, so it can be reused after close. Open → close
        // → reopen, then let the *old* task tear down: with generations it must not delete the
        // new handle (ABA safety).
        let relay = web_relay();
        let mut eng = EngineModel::new();

        let opened = step_frame(&relay, &relay.init(), peer_a(), &open(7, "web"));
        let (key, gen_old) = apply_effects(&mut eng, &opened.effects).expect("registered");

        // Peer closes; the reducer removes it and the engine drops the current handle.
        let closed = step_frame(&relay, &opened.state, peer_a(), &close(7));
        apply_effects(&mut eng, &closed.effects);
        assert!(!eng.map.contains_key(&key));

        // Peer reopens the same id → a new handle with a fresh generation.
        let reopened = step_frame(&relay, &closed.state, peer_a(), &open(7, "web"));
        let (_, gen_new) = apply_effects(&mut eng, &reopened.effects).expect("registered");
        assert_ne!(gen_old, gen_new);

        // The slow OLD task finally tears down with its stale generation: it must neither
        // remove the new handle nor (since this returns false) send the peer a `Close`.
        let removed = eng.close_if_current(&key, gen_old);
        assert!(
            !removed,
            "stale task must not remove — and so must send no peer Close"
        );
        assert_eq!(
            eng.map.get(&key),
            Some(&gen_new),
            "old task must not delete the reopened handle"
        );
    }

    #[test]
    fn engine_model_stays_consistent_with_step_under_interleaving() {
        // Drive Open/Data/Close (peer frames) with *deferred* generation-checked teardowns,
        // asserting the pure session set and the engine model's live keys never diverge.
        let relay = web_relay();
        let peers = [peer_a(), peer_b()];
        let mut state = relay.init();
        let mut eng = EngineModel::new();
        let mut tasks: Vec<(SessionKey, u64)> = Vec::new();
        let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        for _ in 0..3000 {
            let r = next();
            let peer = peers[(r % 2) as usize];
            let session = (r >> 2) & 0x3; // 4 ids → frequent reuse
            match (r >> 8) % 4 {
                0 => {
                    let t = step_frame(&relay, &state, peer, &open(session, "web"));
                    if let Some(task) = apply_effects(&mut eng, &t.effects) {
                        tasks.push(task);
                    }
                    state = t.state;
                }
                1 => {
                    let t = step_frame(&relay, &state, peer, &data(session, b"x"));
                    apply_effects(&mut eng, &t.effects);
                    state = t.state;
                }
                2 => {
                    // Peer close: reducer removes, engine drops current; the matching task is
                    // now stale (its later teardown will be a generation no-op).
                    let t = step_frame(&relay, &state, peer, &close(session));
                    apply_effects(&mut eng, &t.effects);
                    state = t.state;
                }
                _ => {
                    // A deferred task teardown fires. `close_if_current` returns whether this
                    // task was still current; ONLY then does it Untrack and (would) send the
                    // peer a Close. A stale task must do neither — modelled by the `removed`
                    // gate, so a reopened session is never torn down by an old task.
                    if !tasks.is_empty() {
                        let idx = (r >> 16) as usize % tasks.len();
                        let (tkey, tgen) = tasks.swap_remove(idx);
                        let removed = eng.close_if_current(&tkey, tgen);
                        if removed {
                            let untrack = RelayCommand::Untrack {
                                peer: tkey.peer,
                                session: tkey.session,
                                initiator: tkey.initiator,
                            };
                            state = step_command(&relay, &state, &untrack).state;
                        }
                    }
                }
            }
            // The pure session set and the engine's live keys must agree at every step.
            let live: HashSet<SessionKey> = eng.map.keys().cloned().collect();
            assert_eq!(
                state.sessions, live,
                "pure state diverged from the engine model"
            );
        }
    }
}
