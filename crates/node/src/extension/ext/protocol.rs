#![warn(missing_docs)]
//! The pure core of an extension: the [`Protocol`] trait authors implement, its typed
//! step algebra ([`Transition`]), and the decode boundary ([`Wire`] → `Event`).
//!
//! An extension owns **its own** effect algebra (`Protocol::Effect`) — the core defines
//! no global `Effect` enum. Effects are interpreted by the extension's own
//! [`Interpret`](super::Interpret) shell, which is handed a namespace-scoped capability
//! ([`Scope`](super::Scope)) — `send`/`inject` confined to its own namespace. This is what
//! keeps the effect set from becoming a global command bus: a new extension brings its own
//! effects and its own interpreter without ever touching the core.

use bytes::Bytes;
use rings_core::dht::Did;

/// The raw boundary input handed to [`Protocol::decode`]: an inbound message's authenticated
/// sender, this node's own did, and the opaque payload bytes. `decode` turns this into the
/// protocol's typed `Event` (or rejects it).
///
/// `from == me` marks a **self re-injected** message (a local command or an effect's result
/// fed back into the router); any other `from` is a network message from an authenticated
/// peer (a peer cannot forge `from`).
pub struct Wire<'a> {
    /// Authenticated sender of the message.
    pub from: Did,
    /// This node's own did (so `decode` can tell self-injection from a peer).
    pub me: Did,
    /// Opaque payload bytes; the protocol's own codec decides how to read them.
    pub payload: &'a [u8],
}

/// The explicit result of a failed [`Protocol::decode`]: the input is malformed or not for
/// this protocol. The router drops it (a defined no-op) instead of the pure step silently
/// returning an unchanged state — so "valid no-op" and "undecodable input" are
/// distinguishable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reject(pub String);

/// Read-only state carrier passed *into* a step: the protocol's current state `S` plus
/// read-only node facts. The state is borrowed; a step returns the next state in its
/// [`Transition`] rather than mutating in place.
pub struct Ctx<'a, S> {
    /// This node's DID.
    pub did: Did,
    /// Current protocol state (read-only here).
    pub state: &'a S,
}

/// A locally re-injected message: the output of an effect fed back into the router as a
/// fresh inbound, re-decoded by the target namespace's protocol. `Inbound ≅ (Namespace,
/// from, payload)` — the same shape the router takes from the wire, so re-injection and
/// inbound delivery share one path.
///
/// The fields are `pub(crate)`: only the router constructs an `Inbound` (from an interpreter's
/// scoped re-inject, with the namespace and `from` it controls), so an extension shell cannot
/// fabricate one with an arbitrary namespace or a forged remote `from`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Inbound {
    /// Target protocol namespace.
    pub(crate) namespace: String,
    /// Sender to attribute the re-injected message to (`this node` for self-events).
    pub(crate) from: Did,
    /// Payload bytes (re-decoded by the target protocol).
    pub(crate) payload: Bytes,
}

/// The output of a step: the next state and the protocol's own effects to run.
/// `Transition (S, E) ≅ (S, [E])` — the Writer-over-State pair, now parameterized by the
/// protocol's private effect type `E`. `PartialEq`/`Eq` are derived (when `S`/`E` are) so
/// tests can compare whole transitions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition<S, E> {
    /// Next state.
    pub state: S,
    /// Effects to run, in order.
    pub effects: Vec<E>,
}

impl<S, E> Transition<S, E> {
    /// A pure transition with no effects: `pure s = (s, ε)`.
    pub fn pure(state: S) -> Self {
        Self {
            state,
            effects: Vec::new(),
        }
    }

    /// A transition with effects.
    pub fn with(state: S, effects: Vec<E>) -> Self {
        Self { state, effects }
    }
}

/// A protocol: a `namespace`, an initial state, a **decode boundary**, and a state
/// transition that is pure **by contract**.
///
/// ```text
///   init   :              → S
///   decode : Wire        ⇀ Event            (partial: may Reject)
///   step   : (Ctx S, Event) → Transition (S, Effect)
/// ```
///
/// `decode` is the single place raw bytes become a typed `Event`; a malformed/foreign
/// message is an explicit [`Reject`], not a silent no-op in `step`. `step` is then total
/// over well-typed events and pure: no IO, no clocks, no globals. All side effects are
/// described as values of the protocol's **own** `Effect` type and performed by the
/// extension's [`Interpret`](super::Interpret) shell.
pub trait Protocol {
    /// Protocol-private state, owned by the runtime and threaded through `step`.
    type State;
    /// The protocol's typed input, produced by [`decode`](Protocol::decode).
    type Event;
    /// The protocol's **own** effect algebra (the core defines no global effect enum).
    type Effect;

    /// The namespace this protocol is registered and routed under.
    fn namespace(&self) -> &str;

    /// Initial state. `init : 1 → S`.
    fn init(&self) -> Self::State;

    /// Decode the boundary into a typed event, or [`Reject`] it. `decode : Wire ⇀ Event`.
    fn decode(&self, wire: Wire<'_>) -> Result<Self::Event, Reject>;

    /// Pure transition. `step : (Ctx S, Event) → Transition (S, Effect)`.
    fn step(
        &self,
        ctx: Ctx<'_, Self::State>,
        event: Self::Event,
    ) -> Transition<Self::State, Self::Effect>;
}
