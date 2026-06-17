#![warn(missing_docs)]
//! The pure core: the step input/output algebra ([`Event`], [`Effect`], [`Transition`])
//! and the [`Protocol`] trait protocol authors implement.

use std::net::SocketAddr;

use bytes::Bytes;
use rings_core::dht::Did;

use crate::backend::transport::SessionKey;
use crate::backend::transport::TransportKind;

// ── Pure data: input, output, state carrier ───────────────────────────────────

/// The input of a step: a decoded inbound message. Pure data.
#[derive(Clone, Debug)]
pub struct Event {
    /// Sender DID.
    pub from: Did,
    /// Protocol payload (the envelope's payload).
    pub payload: Bytes,
}

/// A *described* side effect — produced by a pure [`Protocol::step`], never executed
/// there. The free algebra of node operations; run by
/// [`Interpreter::run`](super::Interpreter::run).
#[derive(Clone, Debug)]
pub enum Effect {
    /// Send `payload` to `to` under `namespace` over the overlay. Terminal (re-injects
    /// nothing). `Send : (Did, Namespace, Bytes) → IO ε`.
    Send {
        /// Destination DID.
        to: Did,
        /// Destination protocol namespace.
        namespace: String,
        /// Payload bytes.
        payload: Bytes,
    },
    /// Run the impure compute job registered for `namespace` on `input`, then
    /// re-inject its serialized result as a *self*-event (`from = this node`) under
    /// `namespace`. The pure `step` then decides what to do with the result (e.g.
    /// [`Send`](Effect::Send) it). This is the escape hatch for inherently effectful
    /// protocols — e.g. SNARK proving/verification: the heavy, non-pure crypto runs in
    /// the shell ([`Interpreter`](super::Interpreter)), while `step` stays pure, exactly
    /// as transport IO lives in the engine rather than in pure state.
    /// `Compute : (Namespace, Bytes) → IO [Inbound]`.
    Compute {
        /// Namespace whose registered compute job runs (and receives the result).
        namespace: String,
        /// Opaque input bytes; the job's codec is the protocol's own business.
        input: Bytes,
    },
    /// Open a transport-relay session (keyed by [`SessionKey`], which scopes it to the
    /// authenticated `peer`) to `addr` (see [`transport`](crate::backend::transport)).
    /// Interpreted natively; a no-op on browser.
    Connect {
        /// Full session identity `(peer, namespace, session)`.
        key: SessionKey,
        /// Local address to dial.
        addr: SocketAddr,
        /// Stream (TCP) or datagram (UDP) backend.
        kind: TransportKind,
    },
    /// Open a relay session whose local backend is a WebTransport server (`url`), the
    /// browser endpoint counterpart of [`Connect`](Effect::Connect). Interpreted in the
    /// browser; a no-op natively.
    WtConnect {
        /// Full session identity `(peer, namespace, session)`.
        key: SessionKey,
        /// WebTransport server URL to open.
        url: String,
        /// Bidirectional stream (TCP) or datagram (UDP).
        kind: TransportKind,
    },
    /// Write peer-originated bytes to a relay session's local stream. Addressed by the
    /// full [`SessionKey`]; a key whose `peer` did not open the session simply misses.
    Write {
        /// Target session identity.
        key: SessionKey,
        /// Bytes to write locally.
        bytes: Bytes,
    },
    /// Half-close a relay session: shut down its local write side (the peer sent a
    /// FIN), keeping the reverse direction open. No-op for UDP.
    Shutdown {
        /// Session to half-close.
        key: SessionKey,
    },
    /// Close a relay session (full teardown).
    Close {
        /// Session to close.
        key: SessionKey,
    },
    /// Bind a local listener; each accepted connection opens a relay session to `peer`
    /// for `service` under `namespace` (client side). Interpreted natively; a no-op on
    /// browser.
    Listen {
        /// Local address to bind.
        local_addr: SocketAddr,
        /// Peer to relay accepted connections to.
        peer: Did,
        /// Remote service name to open.
        service: String,
        /// Transport namespace the session's frames travel under.
        namespace: String,
        /// Stream (TCP) or datagram (UDP) listener.
        kind: TransportKind,
    },
}

/// Read-only state carrier passed *into* a step: the protocol's current state `S`
/// plus read-only node facts. The state is borrowed; a step returns the next state in
/// its [`Transition`] rather than mutating in place.
pub struct Ctx<'a, S> {
    /// This node's DID.
    pub did: Did,
    /// Current protocol state (read-only here).
    pub state: &'a S,
}

/// A locally re-injected message: an [`Effect`]'s output fed back into the router as
/// a fresh inbound. This is the "event trace" of the Effect monad — running an effect
/// may emit events that re-enter `step`. `Inbound ≅ (Namespace, Event)`.
pub struct Inbound {
    /// Target protocol namespace.
    pub namespace: String,
    /// The re-injected event.
    pub event: Event,
}

/// The output of a step: the next state and the effects to run.
/// `Transition S ≅ (S, [Effect])` — the Writer-over-State pair.
pub struct Transition<S> {
    /// Next state.
    pub state: S,
    /// Effects to run, in order.
    pub effects: Vec<Effect>,
}

impl<S> Transition<S> {
    /// A pure transition with no effects: `pure s = (s, ε)`.
    pub fn pure(state: S) -> Self {
        Self {
            state,
            effects: Vec::new(),
        }
    }

    /// A transition with effects.
    pub fn with(state: S, effects: Vec<Effect>) -> Self {
        Self { state, effects }
    }
}

// ── Pure transition (what protocol authors write) ─────────────────────────────

/// A protocol: a `namespace`, an initial state, and a state transition that is pure
/// **by contract**.
///
/// ```text
///   init :        → S
///   step : (Ctx S, Event) → Transition S
/// ```
///
/// Purity is a *trusted contract*, not enforced by the type system: an implementor
/// could hide interior mutability in `self` or call out to impure code, and a
/// `JsProtocol` (browser) bridges an unrestricted JS function.
/// Authors are expected to keep `step` pure — no IO, no clocks, no globals — and to
/// describe all side effects via the returned [`Effect`]s, which the runtime
/// ([`Interpreter`](super::Interpreter)) performs.
pub trait Protocol {
    /// Protocol-private state, owned by the runtime and threaded through `step`.
    type State;

    /// The namespace this protocol is registered and routed under.
    fn namespace(&self) -> &str;

    /// Initial state. `init : 1 → S`.
    fn init(&self) -> Self::State;

    /// Pure transition. `step : (Ctx S, Event) → Transition S`.
    fn step(&self, ctx: Ctx<'_, Self::State>, event: &Event) -> Transition<Self::State>;
}
