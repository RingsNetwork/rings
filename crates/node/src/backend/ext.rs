#![warn(missing_docs)]
//! Unified, effect-separated protocol abstraction shared by `native` and `browser`.
//!
//! Design: *functional core, imperative shell*. A protocol author writes only a
//! **pure** state transition; all IO (sending, storage) is described as data
//! ([`Effect`]) and executed by the single impure boundary ([`Interpreter`]).
//!
//! Notation (used throughout the doc-comments here):
//!
//! ```text
//!   step : (Ctx S, Event) → Transition S        where   Transition S ≅ (S, [Effect])
//! ```
//!
//! Each event induces, via partial application, an endomorphism on the state paired
//! with an effect log. Such pairs form a monoid (state under endomorphism
//! composition, effects under list concatenation):
//!
//! ```text
//!   (f, a) ⊗ (g, b) = (g ∘ f,  a ⧺ b)            unit = (id, ε)
//! ```
//!
//! so a stream of events is reduced by `mconcat` / `fold` (the Writer-over-State
//! monad). Because `step` is pure it is total, testable and replayable; only
//! [`Interpreter::run`] touches the outside world.
//!
//! The abstraction and constraints are identical on both targets; the sole divergence
//! is the `Send` / `?Send` bound, isolated in [`MaybeSend`] and the usual `cfg_attr`
//! pair.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

use bytes::Bytes;
use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

use crate::backend::transport::SessionId;
use crate::backend::types::BackendMessage;
use crate::error::Error;
use crate::error::Result;
use crate::processor::Processor;

/// Auto-trait bound that is `Send + Sync` on native and empty on browser.
///
/// Lets the pure-core types below be written once; the `Send`-ness divergence
/// (browser futures are not `Send`) is confined here. `∀ T` on browser; `Send + Sync`
/// elsewhere.
#[cfg(not(feature = "browser"))]
pub trait MaybeSend: Send + Sync {}
#[cfg(not(feature = "browser"))]
impl<T: Send + Sync> MaybeSend for T {}
/// Auto-trait bound that is `Send + Sync` on native and empty on browser.
#[cfg(feature = "browser")]
pub trait MaybeSend {}
#[cfg(feature = "browser")]
impl<T> MaybeSend for T {}

// ── Wire ─────────────────────────────────────────────────────────────────────

/// Namespaced message envelope carried over the P2P transport (bincode), in place of
/// the old closed `BackendMessage` enum. `payload` is opaque to the core.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    /// Protocol namespace this payload belongs to.
    pub namespace: String,
    /// Opaque protocol payload; the inner codec is the protocol's own business.
    pub payload: Bytes,
}

impl Envelope {
    /// Build an envelope.
    pub fn new(namespace: impl Into<String>, payload: Bytes) -> Self {
        Self {
            namespace: namespace.into(),
            payload,
        }
    }

    /// Encode for the P2P transport. `encode : Envelope → [u8]`.
    pub fn encode(&self) -> Result<Vec<u8>> {
        bincode::serialize(self).map_err(|_| Error::EncodeError)
    }

    /// Decode from the P2P transport. `decode : [u8] ⇀ Envelope` (partial).
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bincode::deserialize(bytes).map_err(|_| Error::DecodeError)
    }
}

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
/// there. The free algebra of node operations; run by [`Interpreter::run`].
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
    /// Open a transport-relay session to `addr`, streaming it to `peer` under
    /// `namespace` (see [`transport`](crate::backend::transport)). Interpreted
    /// natively; a no-op on browser.
    Connect {
        /// Session id (assigned by the dialing side).
        session: SessionId,
        /// Peer the session relays to/from.
        peer: Did,
        /// Transport namespace the session's frames travel under.
        namespace: String,
        /// Local address to dial.
        addr: SocketAddr,
    },
    /// Write peer-originated bytes to a relay session's local stream.
    Write {
        /// Target session.
        session: SessionId,
        /// Bytes to write locally.
        bytes: Bytes,
    },
    /// Close a relay session.
    Close {
        /// Session to close.
        session: SessionId,
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

/// Upper bound on re-injection iterations per inbound message, so a misbehaving
/// protocol/effect cycle cannot diverge. The driver computes a bounded fixpoint.
const MAX_FIXPOINT_STEPS: u32 = 1024;

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
/// [`JsProtocol`](crate::backend::protocols::js) bridges an unrestricted JS function.
/// Authors are expected to keep `step` pure — no IO, no clocks, no globals — and to
/// describe all side effects via the returned [`Effect`]s, which the runtime
/// ([`Interpreter`]) performs.
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

// ── Imperative shell: the only IO boundary ─────────────────────────────────────

/// Executes [`Effect`]s. The single side-effecting boundary of the extension layer.
#[derive(Clone)]
pub struct Interpreter {
    processor: Arc<Processor>,
    /// Live transport-relay sessions (native only; browser has no raw sockets).
    #[cfg(feature = "node")]
    transport: Arc<crate::backend::transport::engine::TransportSessions>,
}

impl Interpreter {
    /// Build an interpreter over a processor and the shared transport session table.
    #[cfg(feature = "node")]
    pub fn new(
        processor: Arc<Processor>,
        transport: Arc<crate::backend::transport::engine::TransportSessions>,
    ) -> Self {
        Self {
            processor,
            transport,
        }
    }

    /// Build an interpreter over a processor.
    #[cfg(feature = "browser")]
    pub fn new(processor: Arc<Processor>) -> Self {
        Self { processor }
    }

    /// This node's DID (read-only fact handed to `step` via [`Ctx`]).
    pub fn did(&self) -> Did {
        self.processor.did()
    }

    /// Run effects in order, returning any locally re-injected messages (the event
    /// trace fed back into the router). `run : [Effect] → IO [Inbound]`.
    ///
    /// `Send` is terminal (goes to a peer) and re-injects nothing; transport effects
    /// drive native sockets (no-ops on browser). Local reads re-enter the router from
    /// the engine's own tasks rather than via this return value.
    pub async fn run(&self, effects: Vec<Effect>) -> Result<Vec<Inbound>> {
        let reinjected = Vec::new();
        for effect in effects {
            match effect {
                Effect::Send {
                    to,
                    namespace,
                    payload,
                } => {
                    // Transitional: wrap in `BackendMessage::Envelope` so the current
                    // receiver (which decodes `BackendMessage` first) can route it.
                    // Becomes a bare `Envelope` on the wire once `BackendMessage` is
                    // removed.
                    let message = BackendMessage::Envelope(Envelope::new(namespace, payload));
                    self.processor.send_backend_message(to, message).await?;
                }
                Effect::Connect {
                    session,
                    peer,
                    namespace,
                    addr,
                } => {
                    #[cfg(feature = "node")]
                    self.transport
                        .connect(self.processor.clone(), session, peer, namespace, addr)
                        .await;
                    #[cfg(feature = "browser")]
                    {
                        let _ = (session, peer, namespace, addr);
                        tracing::warn!("transport Connect is unsupported on browser");
                    }
                }
                Effect::Write { session, bytes } => {
                    #[cfg(feature = "node")]
                    self.transport.write(session, bytes).await;
                    #[cfg(feature = "browser")]
                    {
                        let _ = (session, bytes);
                        tracing::warn!("transport Write is unsupported on browser");
                    }
                }
                Effect::Close { session } => {
                    #[cfg(feature = "node")]
                    self.transport.close(session);
                    #[cfg(feature = "browser")]
                    {
                        let _ = session;
                        tracing::warn!("transport Close is unsupported on browser");
                    }
                }
            }
        }
        Ok(reinjected)
    }
}

// ── Erasure: wrap a pure Protocol + its state into a uniform handler ───────────

/// Type-erased handler stored in the registry: native is `Send + Sync`, browser not.
#[cfg(not(feature = "browser"))]
pub type DynHandler = dyn Handler + Send + Sync;
/// Type-erased handler stored in the registry.
#[cfg(feature = "browser")]
pub type DynHandler = dyn Handler;

/// Erased, runtime-facing handler. Implemented once, generically, by [`Runner`].
#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
pub trait Handler {
    /// Routed namespace.
    fn namespace(&self) -> &str;
    /// Load state, run the pure step, store the next state, then run its effects,
    /// returning any re-injected messages. `handle : Event → IO [Inbound]`.
    async fn handle(&self, interp: &Interpreter, event: Event) -> Result<Vec<Inbound>>;
}

/// Adapter that owns a protocol's state and drives its pure `step`. This is the
/// imperative shell around the pure core: it performs the state load/store and hands
/// the produced effects to the [`Interpreter`]. Protocol authors never write this.
struct Runner<P: Protocol> {
    protocol: P,
    state: Mutex<P::State>,
}

impl<P: Protocol> Runner<P> {
    fn new(protocol: P) -> Self {
        let state = Mutex::new(protocol.init());
        Self { protocol, state }
    }
}

#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
impl<P> Handler for Runner<P>
where
    P: Protocol + MaybeSend + 'static,
    P::State: MaybeSend + 'static,
{
    fn namespace(&self) -> &str {
        self.protocol.namespace()
    }

    async fn handle(&self, interp: &Interpreter, event: Event) -> Result<Vec<Inbound>> {
        // Pure region: load state, run `step`, store next state. No IO, no await.
        let effects = {
            let mut guard = self.state.lock().map_err(|_| Error::Lock)?;
            let ctx = Ctx {
                did: interp.did(),
                state: guard.deref(),
            };
            let Transition { state, effects } = self.protocol.step(ctx, &event);
            *guard = state;
            effects
        };
        // Impure region: the lock is released; run the described effects, returning
        // any re-injected messages.
        interp.run(effects).await
    }
}

// ── Registry / router ──────────────────────────────────────────────────────────

/// Routes inbound envelopes to protocols by namespace. Cheaply cloneable and shared
/// (interior mutability) so the [`Provider`](crate::provider::Provider) and the
/// inbound callback see the same table.
#[derive(Default, Clone)]
pub struct Extensions {
    handlers: Arc<RwLock<HashMap<String, Arc<DynHandler>>>>,
}

impl Extensions {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pure [`Protocol`] under its namespace (wrapped in a [`Runner`]).
    /// Fails if the registry lock is poisoned.
    pub fn register<P>(&self, protocol: P) -> Result<()>
    where
        P: Protocol + MaybeSend + 'static,
        P::State: MaybeSend + 'static,
    {
        let runner: Arc<DynHandler> = Arc::new(Runner::new(protocol));
        let mut handlers = self.handlers.write().map_err(|_| Error::Lock)?;
        handlers.insert(runner.namespace().to_string(), runner);
        Ok(())
    }

    /// Whether a namespace is registered.
    pub fn contains(&self, namespace: &str) -> bool {
        self.handlers
            .read()
            .map(|h| h.contains_key(namespace))
            .unwrap_or(false)
    }

    fn get(&self, namespace: &str) -> Option<Arc<DynHandler>> {
        self.handlers.read().ok()?.get(namespace).map(Arc::clone)
    }

    /// Route a decoded envelope and drive the re-injection loop to a bounded
    /// fixpoint.
    ///
    /// Starting from the inbound message, repeatedly: route to the namespace's
    /// protocol, run its `step` (pure) and effects (via the interpreter), and
    /// re-enqueue any [`Inbound`]s the effects produced — until the queue drains or
    /// [`MAX_FIXPOINT_STEPS`] is hit. This is the bounded least fixpoint of
    /// `events ↦ ⋃ run(step(event))`.
    ///
    /// Unknown namespaces are logged and dropped (non-fatal): a peer speaking a
    /// protocol this node lacks is expected.
    pub async fn dispatch(
        &self,
        interp: &Interpreter,
        from: Did,
        envelope: Envelope,
    ) -> Result<()> {
        let mut queue: VecDeque<Inbound> = VecDeque::new();
        queue.push_back(Inbound {
            namespace: envelope.namespace,
            event: Event {
                from,
                payload: envelope.payload,
            },
        });

        let mut budget = MAX_FIXPOINT_STEPS;
        while let Some(Inbound { namespace, event }) = queue.pop_front() {
            if budget == 0 {
                return Err(Error::ExtensionError(format!(
                    "fixpoint budget ({MAX_FIXPOINT_STEPS}) exhausted; last namespace {namespace:?}"
                )));
            }
            budget -= 1;

            match self.get(namespace.as_str()) {
                Some(handler) => {
                    let reinjected = handler.handle(interp, event).await?;
                    queue.extend(reinjected);
                }
                None => tracing::debug!(
                    "no protocol registered for namespace {:?}, dropping",
                    namespace
                ),
            }
        }
        Ok(())
    }
}
