#![warn(missing_docs)]
//! Generic transport-relay protocol — the pure server side, shared by TCP and UDP.
//!
//! One [`Protocol`] parameterized by [`TransportKind`] and a namespace; TCP and UDP
//! are the same pure `step`, differing only in the `kind` carried in the emitted
//! `Connect` effect (the engine then dials a stream or a datagram socket). This is the
//! code realization of "TCP/UDP are one abstraction".
//!
//! ```text
//!   S = (services : Name ⇀ SocketAddr,  sessions : ℘ SessionId)
//!   step (Ctx S, Event{from, p}) =
//!     | from = self  ∧  p = RegisterService(n,a)  ↦  (S[services ∪ {n↦a}], ε)
//!     | p = Open(s, n)   ∧  n ∈ services          ↦  (S[sessions ∪ {s}], [Connect s a kind])
//!     | p = Open(s, n)   ∧  n ∉ services          ↦  (S,                  [Send Close s])
//!     | p = Data(s, b)                            ↦  (S,                  [Write s b])
//!     | p = Close(s)                              ↦  (S[sessions ∖ {s}],  [Close s])
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

use crate::backend::ext::Ctx;
use crate::backend::ext::Effect;
use crate::backend::ext::Event;
use crate::backend::ext::Protocol;
use crate::backend::ext::Transition;
use crate::backend::transport::Frame;
use crate::backend::transport::SessionId;
use crate::backend::transport::TransportKind;

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
    /// Currently open sessions.
    sessions: HashSet<SessionId>,
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
    match frame {
        Frame::Open { session, service } => match state.services.get(service.as_str()) {
            Some(addr) => {
                let addr = *addr;
                let mut next = state.clone();
                next.sessions.insert(session);
                Transition::with(next, vec![Effect::Connect {
                    session,
                    peer: from,
                    namespace: namespace.to_string(),
                    addr,
                    kind,
                }])
            }
            None => Transition::with(state.clone(), vec![Effect::Send {
                to: from,
                namespace: namespace.to_string(),
                payload: close_frame(session),
            }]),
        },
        Frame::Data { session, bytes } => {
            Transition::with(state.clone(), vec![Effect::Write { session, bytes }])
        }
        Frame::Shutdown { session } => {
            Transition::with(state.clone(), vec![Effect::Shutdown { session }])
        }
        Frame::Close { session } => {
            let mut next = state.clone();
            next.sessions.remove(&session);
            Transition::with(next, vec![Effect::Close { session }])
        }
    }
}

/// Encode a `Frame::Close` as bytes for a `Send` effect.
fn close_frame(session: SessionId) -> Bytes {
    let frame = Frame::Close { session };
    Bytes::from(bincode::serialize(&frame).unwrap_or_default())
}
