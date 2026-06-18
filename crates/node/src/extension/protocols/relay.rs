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
//! Every session is identified by the **owner-scoped key** `(from, namespace, session)`
//! ([`SessionKey`]), where `from` is the authenticated sender, so no frame can address
//! another peer's session (owner rejection: the engine's keyed lookup misses → reported as
//! "no such session").
//!
//! ```text
//!   S = (services : Name ⇀ T,  sessions : ℘ SessionKey)
//!   k(from,s) = (from, namespace, s)
//!   step (Ctx S, Command(Register n t)) ↦ (S[services∪{n↦t}], ε)            (from = self)
//!   step (Ctx S, Frame(from, Open s n)) ↦ | k∈sessions       → (S, ε)        (duplicate)
//!                                          | n∈services       → (S∪{k}, [Connect k t kind])
//!                                          | otherwise        → (S, [SendClose s])
//!   step (Ctx S, Frame(from, Data s b)) ↦ (S, [Write k b])
//!   step (Ctx S, Frame(from, Close s))  ↦ (S∖{k}, [Close k])
//! ```

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use crate::extension::ext::Core;
use crate::extension::ext::Ctx;
use crate::extension::ext::Inbound;
use crate::extension::ext::Interpret;
use crate::extension::ext::MaybeSend;
use crate::extension::ext::Protocol;
use crate::extension::ext::Reject;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;
use crate::extension::transport::Frame;
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
    /// Engine→protocol feedback: a client-side session was accepted locally and opened to
    /// `peer`. Recorded so `State.sessions` is the **full** authority over live sessions
    /// (client tunnels included), not just remote-opened server sessions.
    Track {
        /// The remote peer this session is tunnelled to.
        peer: Did,
        /// The locally-assigned session id.
        session: SessionId,
    },
    /// Engine→protocol feedback: a session was torn down by the engine (any side); forget
    /// it. The single point through which every teardown reaches the pure state.
    Untrack {
        /// The remote peer of the session.
        peer: Did,
        /// The session id.
        session: SessionId,
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
    /// Reply a `Frame::Close` to a peer that opened an unknown service.
    SendClose {
        /// Peer to reply to.
        to: Did,
        /// Namespace to reply under.
        namespace: String,
        /// Session id to close.
        session: SessionId,
    },
}

/// Relay state: the service registry and the set of open (server-side, remote-opened)
/// sessions. The live OS/WebTransport resources are the interpreter's engine table; this is
/// the protocol's view used for owner-rejection and duplicate-`Open` rejection.
#[derive(Clone)]
pub struct RelayState<T> {
    services: Arc<HashMap<String, T>>,
    sessions: HashSet<SessionKey>,
}

impl<T> Default for RelayState<T> {
    fn default() -> Self {
        Self {
            services: Arc::new(HashMap::new()),
            sessions: HashSet::new(),
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

/// Apply a local [`RelayCommand`]. Pure; mutates the registry / session set, emits no
/// effects. `Track`/`Untrack` are the engine→protocol feedback that make `State.sessions`
/// the full authority over live sessions (both client tunnels and server sessions).
fn step_command<T: Clone>(
    namespace: &str,
    state: &RelayState<T>,
    command: RelayCommand<T>,
) -> Transition<RelayState<T>, RelayEffect<T>> {
    let mut next = state.clone();
    match command {
        RelayCommand::RegisterService { name, target } => {
            Arc::make_mut(&mut next.services).insert(name, target);
        }
        RelayCommand::UnregisterService { name } => {
            Arc::make_mut(&mut next.services).remove(&name);
        }
        RelayCommand::Track { peer, session } => {
            next.sessions
                .insert(SessionKey::new(peer, namespace, session));
        }
        RelayCommand::Untrack { peer, session } => {
            next.sessions
                .remove(&SessionKey::new(peer, namespace, session));
        }
    }
    Transition::pure(next)
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
        Frame::Open { session, service } => {
            let key = SessionKey::new(from, namespace, session);
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
                    namespace: namespace.to_string(),
                    session,
                }]),
            }
        }
        Frame::Data { session, bytes } => {
            let key = SessionKey::new(from, namespace, session);
            Transition::with(state.clone(), vec![RelayEffect::Write { key, bytes }])
        }
        Frame::Shutdown { session } => {
            let key = SessionKey::new(from, namespace, session);
            Transition::with(state.clone(), vec![RelayEffect::Shutdown { key }])
        }
        Frame::Close { session } => {
            let key = SessionKey::new(from, namespace, session);
            let mut next = state.clone();
            next.sessions.remove(&key);
            Transition::with(next, vec![RelayEffect::Close { key }])
        }
    }
}

/// Encode a `Frame::Close` as bytes for an overlay send.
pub(crate) fn close_frame(session: SessionId) -> Bytes {
    let frame = Frame::Close { session };
    Bytes::from(bincode::serialize(&frame).unwrap_or_default())
}

// ── Native interpreter (OS sockets) ───────────────────────────────────────────────────

/// Native relay interpreter: runs [`RelayEffect`]s over the OS-socket engine it owns. The
/// engine uses the [`Core`] capability for both overlay sends and lifecycle feedback
/// (`Track`/`Untrack`), so the engine has no `Processor` of its own.
#[cfg(feature = "node")]
pub struct NativeRelay {
    engine: Arc<crate::extension::transport::engine::TransportSessions>,
}

#[cfg(feature = "node")]
impl NativeRelay {
    /// Build over a shared engine.
    pub fn new(engine: Arc<crate::extension::transport::engine::TransportSessions>) -> Self {
        Self { engine }
    }
}

#[cfg(feature = "node")]
#[async_trait::async_trait]
impl Interpret for NativeRelay {
    type Effect = RelayEffect<std::net::SocketAddr>;

    async fn run(
        &self,
        core: &Core,
        effect: RelayEffect<std::net::SocketAddr>,
    ) -> crate::error::Result<Vec<Inbound>> {
        match effect {
            RelayEffect::Connect { key, target, kind } => {
                self.engine
                    .clone()
                    .connect(core.clone(), key, target, kind)
                    .await;
            }
            RelayEffect::Write { key, bytes } => {
                self.engine.write(&key, bytes).await;
            }
            RelayEffect::Shutdown { key } => {
                self.engine.shutdown(&key).await;
            }
            RelayEffect::Close { key } => {
                self.engine.close(core, &key).await;
            }
            RelayEffect::SendClose {
                to,
                namespace,
                session,
            } => {
                core.send(to, namespace.as_str(), close_frame(session))
                    .await?;
            }
        }
        Ok(Vec::new())
    }
}

// ── Browser interpreter (WebTransport) ────────────────────────────────────────────────

/// Browser relay interpreter: runs [`RelayEffect`]s over the WebTransport engine it owns.
#[cfg(feature = "browser")]
pub struct WtRelay {
    engine: Arc<crate::extension::transport::wt::WtSessions>,
}

#[cfg(feature = "browser")]
impl WtRelay {
    /// Build over a shared WebTransport engine.
    pub fn new(engine: Arc<crate::extension::transport::wt::WtSessions>) -> Self {
        Self { engine }
    }
}

#[cfg(feature = "browser")]
#[async_trait::async_trait(?Send)]
impl Interpret for WtRelay {
    type Effect = RelayEffect<String>;

    async fn run(
        &self,
        core: &Core,
        effect: RelayEffect<String>,
    ) -> crate::error::Result<Vec<Inbound>> {
        match effect {
            RelayEffect::Connect { key, target, kind } => {
                self.engine
                    .clone()
                    .connect(core.clone(), key, target, kind)
                    .await;
            }
            RelayEffect::Write { key, bytes } => {
                self.engine.write(&key, bytes).await;
            }
            RelayEffect::Shutdown { key } => {
                self.engine.shutdown(&key).await;
            }
            RelayEffect::Close { key } => {
                self.engine.close(core, &key).await;
            }
            RelayEffect::SendClose {
                to,
                namespace,
                session,
            } => {
                core.send(to, namespace.as_str(), close_frame(session))
                    .await?;
            }
        }
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::SocketAddr;

    use bytes::Bytes;
    use rings_core::dht::Did;

    use super::close_frame;
    use super::Frame;
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
        let t = step_frame(&relay, &relay.init(), peer_a(), &Frame::Open {
            session: SessionId(7),
            service: "web".to_string(),
        });
        let expected = SessionKey::new(peer_a(), super::TCP, SessionId(7));
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
        let opened = step_frame(&relay, &relay.init(), peer_a(), &Frame::Open {
            session: SessionId(7),
            service: "web".to_string(),
        });
        let key = SessionKey::new(peer_a(), super::TCP, SessionId(7));
        assert!(opened.state.sessions.contains(&key));
        let again = step_frame(&relay, &opened.state, peer_a(), &Frame::Open {
            session: SessionId(7),
            service: "web".to_string(),
        });
        assert!(
            again.effects.is_empty(),
            "duplicate Open must emit no effect"
        );
        assert_eq!(again.state.sessions.len(), 1);
    }

    #[test]
    fn open_unknown_service_closes_and_records_nothing() {
        let relay = web_relay();
        let t = step_frame(&relay, &relay.init(), peer_a(), &Frame::Open {
            session: SessionId(7),
            service: "ssh".to_string(),
        });
        match t.effects.as_slice() {
            [RelayEffect::SendClose {
                to,
                namespace,
                session,
            }] => {
                assert_eq!(*to, peer_a());
                assert_eq!(namespace, super::TCP);
                assert_eq!(*session, SessionId(7));
                // The wire bytes a peer would receive.
                assert_eq!(close_frame(*session), close_frame(SessionId(7)));
            }
            other => panic!("expected one SendClose, got {other:?}"),
        }
        assert!(t.state.sessions.is_empty());
    }

    #[test]
    fn data_writes_to_the_keyed_session() {
        let relay = web_relay();
        let t = step_frame(&relay, &relay.init(), peer_a(), &Frame::Data {
            session: SessionId(7),
            bytes: Bytes::from_static(b"hello"),
        });
        let expected = SessionKey::new(peer_a(), super::TCP, SessionId(7));
        match t.effects.as_slice() {
            [RelayEffect::Write { key, bytes }] => {
                assert_eq!(*key, expected);
                assert_eq!(bytes.as_ref(), b"hello");
            }
            other => panic!("expected one Write, got {other:?}"),
        }
    }

    #[test]
    fn close_removes_the_session_and_emits_close() {
        let relay = web_relay();
        let opened = step_frame(&relay, &relay.init(), peer_a(), &Frame::Open {
            session: SessionId(7),
            service: "web".to_string(),
        });
        let t = step_frame(&relay, &opened.state, peer_a(), &Frame::Close {
            session: SessionId(7),
        });
        let expected = SessionKey::new(peer_a(), super::TCP, SessionId(7));
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
        let t = step_frame(&relay, &registered.state, peer_a(), &Frame::Open {
            session: SessionId(1),
            service: "web".to_string(),
        });
        match t.effects.as_slice() {
            [RelayEffect::Connect { target, .. }] => assert_eq!(*target, web_addr()),
            other => panic!("expected one Connect, got {other:?}"),
        }
    }

    #[test]
    fn track_and_untrack_make_state_the_authority_over_client_sessions() {
        let relay = web_relay();
        // A client-side accept is fed back as `Track` (the engine→protocol feedback): the
        // session is recorded in State even though no peer `Open` frame created it.
        let tracked = step_command(&relay, &relay.init(), &RelayCommand::Track {
            peer: peer_a(),
            session: SessionId(9),
        });
        assert!(tracked.effects.is_empty(), "Track is a pure state update");
        let key = SessionKey::new(peer_a(), super::TCP, SessionId(9));
        assert!(tracked.state.sessions.contains(&key));

        // Teardown feeds back `Untrack`, removing it — so State stays the full authority.
        let untracked = step_command(&relay, &tracked.state, &RelayCommand::Untrack {
            peer: peer_a(),
            session: SessionId(9),
        });
        assert!(untracked.effects.is_empty());
        assert!(!untracked.state.sessions.contains(&key));
    }

    #[test]
    fn a_peer_cannot_address_another_peers_session() {
        let relay = web_relay();
        let a_open = step_frame(&relay, &relay.init(), peer_a(), &Frame::Open {
            session: SessionId(0),
            service: "web".to_string(),
        });
        let key_a = SessionKey::new(peer_a(), super::TCP, SessionId(0));
        assert!(a_open.state.sessions.contains(&key_a));
        let key_b = SessionKey::new(peer_b(), super::TCP, SessionId(0));
        assert_ne!(key_a, key_b);

        let b_data = step_frame(&relay, &a_open.state, peer_b(), &Frame::Data {
            session: SessionId(0),
            bytes: Bytes::from_static(b"x"),
        });
        match b_data.effects.as_slice() {
            [RelayEffect::Write { key, .. }] => assert_eq!(*key, key_b),
            other => panic!("expected one Write, got {other:?}"),
        }
        let b_close = step_frame(&relay, &a_open.state, peer_b(), &Frame::Close {
            session: SessionId(0),
        });
        match b_close.effects.as_slice() {
            [RelayEffect::Close { key }] => assert_eq!(*key, key_b),
            other => panic!("expected one Close, got {other:?}"),
        }
        assert!(b_close.state.sessions.contains(&key_a));
    }
}
