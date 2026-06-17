#![warn(missing_docs)]
//! Generic transport-relay protocol — the pure server side, shared by TCP and UDP.
//!
//! One [`Protocol`] parameterized by [`TransportKind`] and a namespace; TCP and UDP
//! are the same pure `step`, differing only in the `kind` carried in the emitted
//! `Connect` effect (the engine then dials a stream or a datagram socket). This is the
//! code realization of "TCP/UDP are one abstraction".
//!
//! Every session is identified not by the bare wire id `s` but by the **owner-scoped key**
//! `k = (from, namespace, s)` (a [`SessionKey`]), where `from` is the authenticated sender.
//! Because a peer cannot forge `from`, the keys it can name are exactly those whose owner is
//! itself — so no frame can ever write to or close another peer's session (owner rejection).
//!
//! ```text
//!   S = (services : Name ⇀ SocketAddr,  sessions : ℘ SessionKey)
//!   k(from, s) = (from, namespace, s)
//!   step (Ctx S, Event{from, p}) =
//!     | from = self  ∧  p = RegisterService(n,a)  ↦  (S[services ∪ {n↦a}], ε)
//!     | p = Open(s, n)   ∧  n ∈ services          ↦  (S[sessions ∪ {k(from,s)}], [Connect k a kind])
//!     | p = Open(s, n)   ∧  n ∉ services          ↦  (S,                  [Send Close s])
//!     | p = Data(s, b)                            ↦  (S,                  [Write k(from,s) b])
//!     | p = Shutdown(s)                           ↦  (S,                  [Shutdown k(from,s)])
//!     | p = Close(s)                              ↦  (S[sessions ∖ {k(from,s)}],  [Close k(from,s)])
//! ```
//!
//! Provenance distinguishes the two payload codecs purely: `from = self` is a locally
//! re-injected [`Command`] (runtime registration); any other `from` is a network
//! [`Frame`] (peers cannot forge `from`, it is the verified signer). So the service
//! registry supports both fixed config (via [`Relay::tcp`]/[`Relay::udp`]) and runtime
//! registration without leaving the pure model.

use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

use crate::extension::ext::Ctx;
use crate::extension::ext::Effect;
use crate::extension::ext::Event;
use crate::extension::ext::Protocol;
use crate::extension::ext::Transition;
use crate::extension::transport::Frame;
use crate::extension::transport::SessionId;
use crate::extension::transport::SessionKey;
use crate::extension::transport::TransportKind;

/// Namespace for the TCP relay.
pub const TCP: &str = "tcp";
/// Namespace for the UDP relay.
pub const UDP: &str = "udp";

/// A local control command, re-injected by the provider (never sent by peers).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    /// Map a service name to a local address that `Open` may dial.
    RegisterService {
        /// Service name.
        name: String,
        /// Local address to dial for this service.
        addr: SocketAddr,
    },
    /// Remove a service mapping.
    UnregisterService {
        /// Service name to remove.
        name: String,
    },
}

/// Relay state: the service registry and the set of open sessions.
#[derive(Clone, Default)]
pub struct State {
    /// Service name → local address. `Arc` so the hot data path clones cheaply.
    services: Arc<HashMap<String, SocketAddr>>,
    /// Sessions this relay has opened (as server), keyed by their full
    /// [`SessionKey`] so a session is always scoped to the peer that opened it.
    sessions: HashSet<SessionKey>,
}

/// Transport relay protocol (server side), parameterized by kind + namespace.
#[derive(Clone)]
pub struct Relay {
    namespace: String,
    kind: TransportKind,
    config: HashMap<String, SocketAddr>,
}

impl Relay {
    /// A TCP relay with a fixed service configuration.
    pub fn tcp(config: HashMap<String, SocketAddr>) -> Self {
        Self {
            namespace: TCP.to_string(),
            kind: TransportKind::Tcp,
            config,
        }
    }

    /// A UDP relay with a fixed service configuration.
    pub fn udp(config: HashMap<String, SocketAddr>) -> Self {
        Self {
            namespace: UDP.to_string(),
            kind: TransportKind::Udp,
            config,
        }
    }
}

impl Protocol for Relay {
    type State = State;

    fn namespace(&self) -> &str {
        self.namespace.as_str()
    }

    fn init(&self) -> State {
        State {
            services: Arc::new(self.config.clone()),
            sessions: HashSet::new(),
        }
    }

    fn step(&self, ctx: Ctx<'_, State>, event: &Event) -> Transition<State> {
        // Local re-injection (provenance = self): a control command.
        if event.from == ctx.did {
            return step_command(ctx.state, event.payload.as_ref());
        }
        // Otherwise a network frame from a peer.
        step_frame(
            self.kind,
            self.namespace.as_str(),
            ctx.state,
            event.from,
            event.payload.as_ref(),
        )
    }
}

/// Apply a local [`Command`]. Pure; only mutates the registry, emits no effects.
fn step_command(state: &State, payload: &[u8]) -> Transition<State> {
    let Ok(command) = bincode::deserialize::<Command>(payload) else {
        return Transition::pure(state.clone());
    };
    let mut next = state.clone();
    match command {
        Command::RegisterService { name, addr } => {
            Arc::make_mut(&mut next.services).insert(name, addr);
        }
        Command::UnregisterService { name } => {
            Arc::make_mut(&mut next.services).remove(&name);
        }
    }
    Transition::pure(next)
}

/// Apply a network [`Frame`]. Pure; emits transport effects.
fn step_frame(
    kind: TransportKind,
    namespace: &str,
    state: &State,
    from: Did,
    payload: &[u8],
) -> Transition<State> {
    let Ok(frame) = bincode::deserialize::<Frame>(payload) else {
        return Transition::pure(state.clone());
    };
    // `from` is the authenticated sender, so the key scopes every effect to this peer: a
    // peer can only ever name sessions whose `peer` is itself (owner rejection — a frame
    // for someone else's session resolves to a key the engine never registered).
    match frame {
        Frame::Open { session, service } => {
            let key = SessionKey::new(from, namespace, session);
            // Reject a duplicate/retried Open for a session this peer already holds open:
            // re-emitting Connect would spawn a second relay task for the same key, and the
            // engine would replace the live handle (risking the old task's later teardown
            // closing the new session). Leave the existing session untouched.
            if state.sessions.contains(&key) {
                return Transition::pure(state.clone());
            }
            match state.services.get(service.as_str()) {
                Some(addr) => {
                    let addr = *addr;
                    let mut next = state.clone();
                    next.sessions.insert(key.clone());
                    Transition::with(next, vec![Effect::Connect { key, addr, kind }])
                }
                None => Transition::with(state.clone(), vec![Effect::Send {
                    to: from,
                    namespace: namespace.to_string(),
                    payload: close_frame(session),
                }]),
            }
        }
        Frame::Data { session, bytes } => {
            let key = SessionKey::new(from, namespace, session);
            Transition::with(state.clone(), vec![Effect::Write { key, bytes }])
        }
        Frame::Shutdown { session } => {
            let key = SessionKey::new(from, namespace, session);
            Transition::with(state.clone(), vec![Effect::Shutdown { key }])
        }
        Frame::Close { session } => {
            let key = SessionKey::new(from, namespace, session);
            let mut next = state.clone();
            next.sessions.remove(&key);
            Transition::with(next, vec![Effect::Close { key }])
        }
    }
}

/// Encode a `Frame::Close` as bytes for a `Send` effect.
fn close_frame(session: SessionId) -> Bytes {
    let frame = Frame::Close { session };
    Bytes::from(bincode::serialize(&frame).unwrap_or_default())
}

/// Browser relay: same pure `step` as [`Relay`], but a service resolves to a
/// WebTransport **URL** (the browser endpoint) and `Open` emits
/// [`Effect::WtConnect`] instead of `Connect`. See
/// [`wt`](crate::extension::transport::wt).
#[cfg(feature = "browser")]
pub use wt_relay::Command as WtCommand;
#[cfg(feature = "browser")]
pub use wt_relay::WtRelay;

#[cfg(feature = "browser")]
mod wt_relay {
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::Arc;

    use rings_core::dht::Did;
    use serde::Deserialize;
    use serde::Serialize;

    use super::TCP;
    use super::UDP;
    use crate::extension::ext::Ctx;
    use crate::extension::ext::Effect;
    use crate::extension::ext::Event;
    use crate::extension::ext::Protocol;
    use crate::extension::ext::Transition;
    use crate::extension::transport::Frame;
    use crate::extension::transport::SessionKey;
    use crate::extension::transport::TransportKind;

    /// Local control command for the browser relay (service name → WebTransport URL).
    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub enum Command {
        /// Map a service name to a WebTransport URL that `Open` may dial.
        RegisterService {
            /// Service name.
            name: String,
            /// WebTransport URL.
            url: String,
        },
        /// Remove a service mapping.
        UnregisterService {
            /// Service name to remove.
            name: String,
        },
    }

    /// Browser relay state: service → WebTransport URL, plus open sessions keyed by their
    /// full [`SessionKey`] (peer-scoped, like the native relay).
    #[derive(Clone, Default)]
    pub struct State {
        services: Arc<HashMap<String, String>>,
        sessions: HashSet<SessionKey>,
    }

    /// Browser relay protocol (WebTransport endpoint), parameterized by kind + namespace.
    #[derive(Clone)]
    pub struct WtRelay {
        namespace: String,
        kind: TransportKind,
        config: HashMap<String, String>,
    }

    impl WtRelay {
        /// A browser TCP relay (WebTransport bidi streams).
        pub fn tcp(config: HashMap<String, String>) -> Self {
            Self {
                namespace: TCP.to_string(),
                kind: TransportKind::Tcp,
                config,
            }
        }

        /// A browser UDP relay (WebTransport datagrams).
        pub fn udp(config: HashMap<String, String>) -> Self {
            Self {
                namespace: UDP.to_string(),
                kind: TransportKind::Udp,
                config,
            }
        }
    }

    impl Protocol for WtRelay {
        type State = State;

        fn namespace(&self) -> &str {
            self.namespace.as_str()
        }

        fn init(&self) -> State {
            State {
                services: Arc::new(self.config.clone()),
                sessions: HashSet::new(),
            }
        }

        fn step(&self, ctx: Ctx<'_, State>, event: &Event) -> Transition<State> {
            if event.from == ctx.did {
                return step_command(ctx.state, event.payload.as_ref());
            }
            step_frame(
                self.kind,
                self.namespace.as_str(),
                ctx.state,
                event.from,
                event.payload.as_ref(),
            )
        }
    }

    fn step_command(state: &State, payload: &[u8]) -> Transition<State> {
        let Ok(command) = bincode::deserialize::<Command>(payload) else {
            return Transition::pure(state.clone());
        };
        let mut next = state.clone();
        match command {
            Command::RegisterService { name, url } => {
                Arc::make_mut(&mut next.services).insert(name, url);
            }
            Command::UnregisterService { name } => {
                Arc::make_mut(&mut next.services).remove(&name);
            }
        }
        Transition::pure(next)
    }

    fn step_frame(
        kind: TransportKind,
        namespace: &str,
        state: &State,
        from: Did,
        payload: &[u8],
    ) -> Transition<State> {
        let Ok(frame) = bincode::deserialize::<Frame>(payload) else {
            return Transition::pure(state.clone());
        };
        match frame {
            Frame::Open { session, service } => {
                let key = SessionKey::new(from, namespace, session);
                // Reject a duplicate/retried Open (see the native relay for the rationale).
                if state.sessions.contains(&key) {
                    return Transition::pure(state.clone());
                }
                match state.services.get(service.as_str()) {
                    Some(url) => {
                        let url = url.clone();
                        let mut next = state.clone();
                        next.sessions.insert(key.clone());
                        Transition::with(next, vec![Effect::WtConnect { key, url, kind }])
                    }
                    None => Transition::with(state.clone(), vec![Effect::Send {
                        to: from,
                        namespace: namespace.to_string(),
                        payload: super::close_frame(session),
                    }]),
                }
            }
            Frame::Data { session, bytes } => {
                let key = SessionKey::new(from, namespace, session);
                Transition::with(state.clone(), vec![Effect::Write { key, bytes }])
            }
            Frame::Shutdown { session } => {
                let key = SessionKey::new(from, namespace, session);
                Transition::with(state.clone(), vec![Effect::Shutdown { key }])
            }
            Frame::Close { session } => {
                let key = SessionKey::new(from, namespace, session);
                let mut next = state.clone();
                next.sessions.remove(&key);
                Transition::with(next, vec![Effect::Close { key }])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::SocketAddr;

    use bytes::Bytes;
    use rings_core::dht::Did;

    use super::close_frame;
    use super::Command;
    use super::Ctx;
    use super::Effect;
    use super::Event;
    use super::Frame;
    use super::Protocol;
    use super::Relay;
    use super::SessionId;
    use super::SessionKey;
    use super::Transition;
    use super::TransportKind;

    /// The node running the relay.
    fn this_node() -> Did {
        Did::from(1u32)
    }

    /// Some other, authenticated peer.
    fn peer_a() -> Did {
        Did::from(2u32)
    }

    /// A second authenticated peer, distinct from [`peer_a`].
    fn peer_b() -> Did {
        Did::from(3u32)
    }

    fn web_addr() -> SocketAddr {
        "127.0.0.1:8080".parse().unwrap()
    }

    /// A TCP relay that maps `web` → [`web_addr`].
    fn web_relay() -> Relay {
        let mut config = HashMap::new();
        config.insert("web".to_string(), web_addr());
        Relay::tcp(config)
    }

    fn frame_event(from: Did, frame: &Frame) -> Event {
        Event {
            from,
            payload: Bytes::from(bincode::serialize(frame).unwrap()),
        }
    }

    fn command_event(from: Did, command: &Command) -> Event {
        Event {
            from,
            payload: Bytes::from(bincode::serialize(command).unwrap()),
        }
    }

    /// Drive one `step` against the given state, returning the transition.
    fn step(relay: &Relay, state: &super::State, event: &Event) -> Transition<super::State> {
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
        let event = frame_event(peer_a(), &Frame::Open {
            session: SessionId(7),
            service: "web".to_string(),
        });
        let transition = step(&relay, &relay.init(), &event);

        let expected = SessionKey::new(peer_a(), super::TCP, SessionId(7));
        match transition.effects.as_slice() {
            [Effect::Connect { key, addr, kind }] => {
                assert_eq!(*key, expected);
                assert_eq!(*addr, web_addr());
                assert!(matches!(kind, TransportKind::Tcp));
            }
            other => panic!("expected a single Connect, got {other:?}"),
        }
        assert!(transition.state.sessions.contains(&expected));
    }

    #[test]
    fn duplicate_open_for_a_live_session_is_rejected() {
        let relay = web_relay();
        let opened = step(
            &relay,
            &relay.init(),
            &frame_event(peer_a(), &Frame::Open {
                session: SessionId(7),
                service: "web".to_string(),
            }),
        );
        let key = SessionKey::new(peer_a(), super::TCP, SessionId(7));
        assert!(opened.state.sessions.contains(&key));

        // A second Open for the same (peer, namespace, session) must not emit another
        // Connect (which would spawn a duplicate relay task) and must leave the session
        // set unchanged.
        let again = step(
            &relay,
            &opened.state,
            &frame_event(peer_a(), &Frame::Open {
                session: SessionId(7),
                service: "web".to_string(),
            }),
        );
        assert!(
            again.effects.is_empty(),
            "duplicate Open must emit no effect"
        );
        assert!(again.state.sessions.contains(&key));
        assert_eq!(again.state.sessions.len(), 1);
    }

    #[test]
    fn open_unknown_service_closes_and_records_nothing() {
        let relay = web_relay();
        let event = frame_event(peer_a(), &Frame::Open {
            session: SessionId(7),
            service: "ssh".to_string(),
        });
        let transition = step(&relay, &relay.init(), &event);

        match transition.effects.as_slice() {
            [Effect::Send {
                to,
                namespace,
                payload,
            }] => {
                assert_eq!(*to, peer_a());
                assert_eq!(namespace, super::TCP);
                assert_eq!(*payload, close_frame(SessionId(7)));
            }
            other => panic!("expected a single Send(Close), got {other:?}"),
        }
        assert!(transition.state.sessions.is_empty());
    }

    #[test]
    fn data_writes_to_the_keyed_session() {
        let relay = web_relay();
        let event = frame_event(peer_a(), &Frame::Data {
            session: SessionId(7),
            bytes: Bytes::from_static(b"hello"),
        });
        let transition = step(&relay, &relay.init(), &event);

        let expected = SessionKey::new(peer_a(), super::TCP, SessionId(7));
        match transition.effects.as_slice() {
            [Effect::Write { key, bytes }] => {
                assert_eq!(*key, expected);
                assert_eq!(bytes.as_ref(), b"hello");
            }
            other => panic!("expected a single Write, got {other:?}"),
        }
    }

    #[test]
    fn shutdown_half_closes_the_keyed_session() {
        let relay = web_relay();
        let event = frame_event(peer_a(), &Frame::Shutdown {
            session: SessionId(7),
        });
        let transition = step(&relay, &relay.init(), &event);

        let expected = SessionKey::new(peer_a(), super::TCP, SessionId(7));
        match transition.effects.as_slice() {
            [Effect::Shutdown { key }] => assert_eq!(*key, expected),
            other => panic!("expected a single Shutdown, got {other:?}"),
        }
    }

    #[test]
    fn close_removes_the_session_and_emits_close() {
        let relay = web_relay();
        // Open first so the session is recorded, then close it.
        let opened = step(
            &relay,
            &relay.init(),
            &frame_event(peer_a(), &Frame::Open {
                session: SessionId(7),
                service: "web".to_string(),
            }),
        );
        let transition = step(
            &relay,
            &opened.state,
            &frame_event(peer_a(), &Frame::Close {
                session: SessionId(7),
            }),
        );

        let expected = SessionKey::new(peer_a(), super::TCP, SessionId(7));
        match transition.effects.as_slice() {
            [Effect::Close { key }] => assert_eq!(*key, expected),
            other => panic!("expected a single Close, got {other:?}"),
        }
        assert!(!transition.state.sessions.contains(&expected));
    }

    #[test]
    fn register_service_via_self_command_then_open_connects() {
        // No fixed config; the service is registered at runtime by a self-event.
        let relay = Relay::tcp(HashMap::new());
        let registered = step(
            &relay,
            &relay.init(),
            &command_event(this_node(), &Command::RegisterService {
                name: "web".to_string(),
                addr: web_addr(),
            }),
        );
        assert!(registered.effects.is_empty());

        let transition = step(
            &relay,
            &registered.state,
            &frame_event(peer_a(), &Frame::Open {
                session: SessionId(1),
                service: "web".to_string(),
            }),
        );
        match transition.effects.as_slice() {
            [Effect::Connect { addr, .. }] => assert_eq!(*addr, web_addr()),
            other => panic!("expected a single Connect, got {other:?}"),
        }
    }

    /// Owner rejection: the key is built from the *authenticated* sender, so two peers
    /// that pick the same wire `SessionId` produce **distinct** keys, and one peer can
    /// neither write to nor close another peer's session.
    #[test]
    fn a_peer_cannot_address_another_peers_session() {
        let relay = web_relay();
        // peer A opens session id 0.
        let a_open = step(
            &relay,
            &relay.init(),
            &frame_event(peer_a(), &Frame::Open {
                session: SessionId(0),
                service: "web".to_string(),
            }),
        );
        let key_a = SessionKey::new(peer_a(), super::TCP, SessionId(0));
        assert!(a_open.state.sessions.contains(&key_a));

        // peer B sends Data/Close for the *same wire id* 0. Its key is scoped to B.
        let key_b = SessionKey::new(peer_b(), super::TCP, SessionId(0));
        assert_ne!(key_a, key_b);

        let b_data = step(
            &relay,
            &a_open.state,
            &frame_event(peer_b(), &Frame::Data {
                session: SessionId(0),
                bytes: Bytes::from_static(b"x"),
            }),
        );
        match b_data.effects.as_slice() {
            [Effect::Write { key, .. }] => assert_eq!(*key, key_b),
            other => panic!("expected a single Write, got {other:?}"),
        }

        // B closing "session 0" removes only B's key; A's session survives.
        let b_close = step(
            &relay,
            &a_open.state,
            &frame_event(peer_b(), &Frame::Close {
                session: SessionId(0),
            }),
        );
        match b_close.effects.as_slice() {
            [Effect::Close { key }] => assert_eq!(*key, key_b),
            other => panic!("expected a single Close, got {other:?}"),
        }
        assert!(b_close.state.sessions.contains(&key_a));
    }
}
