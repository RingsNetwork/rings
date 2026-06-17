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
use std::ops::Deref;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

use bytes::Bytes;
use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

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
    /// Send `payload` to `to` under `namespace`.
    Send {
        /// Destination DID.
        to: Did,
        /// Destination protocol namespace.
        namespace: String,
        /// Payload bytes.
        payload: Bytes,
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

/// A protocol: a `namespace`, an initial state, and a **pure** state transition.
///
/// ```text
///   init :        → S
///   step : (Ctx S, Event) → Transition S
/// ```
///
/// `step` MUST be pure (no IO, no clocks, no globals). Describe all side effects via
/// the returned [`Effect`]s; the runtime ([`Interpreter`]) performs them.
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
}

impl Interpreter {
    /// Build an interpreter over a processor.
    pub fn new(processor: Arc<Processor>) -> Self {
        Self { processor }
    }

    /// This node's DID (read-only fact handed to `step` via [`Ctx`]).
    pub fn did(&self) -> Did {
        self.processor.did()
    }

    /// Run effects in order. `run : [Effect] → IO ()`.
    pub async fn run(&self, effects: Vec<Effect>) -> Result<()> {
        for effect in effects {
            match effect {
                Effect::Send {
                    to,
                    namespace,
                    payload,
                } => {
                    let bytes = Envelope::new(namespace, payload).encode()?;
                    self.processor.send_message(to, bytes.as_slice()).await?;
                }
            }
        }
        Ok(())
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
    /// Load state, run the pure step, store the next state, then run its effects.
    async fn handle(&self, interp: &Interpreter, event: Event) -> Result<()>;
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

    async fn handle(&self, interp: &Interpreter, event: Event) -> Result<()> {
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
        // Impure region: the lock is released; run the described effects.
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
    pub fn register<P>(&self, protocol: P)
    where
        P: Protocol + MaybeSend + 'static,
        P::State: MaybeSend + 'static,
    {
        let runner: Arc<DynHandler> = Arc::new(Runner::new(protocol));
        if let Ok(mut handlers) = self.handlers.write() {
            handlers.insert(runner.namespace().to_string(), runner);
        }
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

    /// Route a decoded envelope. Unknown namespaces are logged and dropped
    /// (non-fatal): a peer speaking a protocol this node lacks is expected.
    pub async fn dispatch(
        &self,
        interp: &Interpreter,
        from: Did,
        envelope: Envelope,
    ) -> Result<()> {
        match self.get(envelope.namespace.as_str()) {
            Some(handler) => {
                let event = Event {
                    from,
                    payload: envelope.payload,
                };
                handler.handle(interp, event).await
            }
            None => {
                tracing::debug!(
                    "no protocol registered for namespace {:?}, dropping",
                    envelope.namespace
                );
                Ok(())
            }
        }
    }
}
