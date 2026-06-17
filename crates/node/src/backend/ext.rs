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
#[cfg(not(feature = "browser"))]
use futures::future::BoxFuture;
#[cfg(feature = "browser")]
use futures::future::LocalBoxFuture;
use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

use crate::backend::transport::SessionKey;
use crate::backend::transport::TransportKind;
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
    /// Run the impure compute job registered for `namespace` on `input`, then
    /// re-inject its serialized result as a *self*-event (`from = this node`) under
    /// `namespace`. The pure `step` then decides what to do with the result (e.g.
    /// [`Send`](Effect::Send) it). This is the escape hatch for inherently effectful
    /// protocols — e.g. SNARK proving/verification: the heavy, non-pure crypto runs in
    /// the shell ([`Interpreter`]), while `step` stays pure, exactly as transport IO
    /// lives in the engine rather than in pure state.
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
/// `JsProtocol` (browser) bridges an unrestricted JS function.
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

// ── Compute services: registered impure jobs (the effectful escape hatch) ──────

/// An impure compute job for a namespace: `input ↦ IO output`. Runs in the shell
/// when an [`Effect::Compute`] is interpreted; its `output` is re-injected as a
/// self-event. Used by protocols whose work is genuinely non-pure (e.g. SNARK
/// proving), keeping their `step` pure. `Send + Sync` natively, neither in browser.
#[cfg(not(feature = "browser"))]
pub type ComputeFn = Arc<dyn Fn(Bytes) -> BoxFuture<'static, Result<Bytes>> + Send + Sync>;
/// An impure compute job for a namespace: `input ↦ IO output`.
#[cfg(feature = "browser")]
pub type ComputeFn = Arc<dyn Fn(Bytes) -> LocalBoxFuture<'static, Result<Bytes>>>;

/// Registry of [`ComputeFn`]s keyed by namespace. Shared (interior mutability) so a
/// protocol registered on the [`Provider`](crate::provider::Provider) and the
/// [`Interpreter`] that runs its effects see the same jobs.
#[derive(Default, Clone)]
pub struct ComputeServices {
    jobs: Arc<RwLock<HashMap<String, ComputeFn>>>,
}

impl ComputeServices {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the compute job for `namespace`. Fails if the lock is poisoned.
    pub fn register(&self, namespace: impl Into<String>, job: ComputeFn) -> Result<()> {
        let mut jobs = self.jobs.write().map_err(|_| Error::Lock)?;
        jobs.insert(namespace.into(), job);
        Ok(())
    }

    fn get(&self, namespace: &str) -> Option<ComputeFn> {
        self.jobs.read().ok()?.get(namespace).map(Arc::clone)
    }
}

// ── Imperative shell: the only IO boundary ─────────────────────────────────────

/// Executes [`Effect`]s. The single side-effecting boundary of the extension layer.
///
/// It holds the platform transport engine that owns live relay endpoints: OS sockets
/// natively ([`engine::TransportSessions`](crate::backend::transport::engine)), or
/// WebTransport sessions in the browser
/// (`wt::WtSessions`, browser). `Write`/`Shutdown`/`Close`
/// dispatch to it on both targets; only the *open* effect differs (`Connect`/`Listen`
/// natively, `WtConnect` in the browser).
#[derive(Clone)]
pub struct Interpreter {
    processor: Arc<Processor>,
    compute: ComputeServices,
    #[cfg(feature = "node")]
    transport: Arc<crate::backend::transport::engine::TransportSessions>,
    #[cfg(feature = "browser")]
    transport: Arc<crate::backend::transport::wt::WtSessions>,
}

impl Interpreter {
    /// Build an interpreter over a processor and the shared native transport engine.
    #[cfg(feature = "node")]
    pub fn new(
        processor: Arc<Processor>,
        transport: Arc<crate::backend::transport::engine::TransportSessions>,
        compute: ComputeServices,
    ) -> Self {
        Self {
            processor,
            compute,
            transport,
        }
    }

    /// Build an interpreter over a processor and the shared browser WebTransport engine.
    #[cfg(feature = "browser")]
    pub fn new(
        processor: Arc<Processor>,
        transport: Arc<crate::backend::transport::wt::WtSessions>,
        compute: ComputeServices,
    ) -> Self {
        Self {
            processor,
            compute,
            transport,
        }
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
        let mut reinjected = Vec::new();
        for effect in effects {
            match effect {
                Effect::Compute { namespace, input } => {
                    // Impure region: run the namespace's registered job, then feed its
                    // result back as a self-event so the pure `step` can act on it.
                    match self.compute.get(namespace.as_str()) {
                        Some(job) => {
                            let output = job(input).await?;
                            reinjected.push(Inbound {
                                namespace: namespace.clone(),
                                event: Event {
                                    from: self.did(),
                                    payload: output,
                                },
                            });
                        }
                        None => tracing::warn!(
                            "no compute service registered for namespace {:?}, dropping",
                            namespace
                        ),
                    }
                }
                Effect::Send {
                    to,
                    namespace,
                    payload,
                } => {
                    let envelope = Envelope::new(namespace, payload);
                    self.processor.send_envelope(to, &envelope).await?;
                }
                Effect::Connect { key, addr, kind } => {
                    #[cfg(feature = "node")]
                    self.transport
                        .clone()
                        .connect(self.processor.clone(), key, addr, kind)
                        .await;
                    #[cfg(feature = "browser")]
                    {
                        let _ = (key, addr, kind);
                        tracing::warn!("transport Connect (socket) is unsupported on browser");
                    }
                }
                Effect::WtConnect { key, url, kind } => {
                    #[cfg(feature = "browser")]
                    self.transport
                        .clone()
                        .connect(self.processor.clone(), key, url, kind)
                        .await;
                    #[cfg(not(feature = "browser"))]
                    {
                        let _ = (key, url, kind);
                        tracing::warn!("WtConnect (WebTransport) is only supported on browser");
                    }
                }
                // Write/Shutdown/Close address an existing session by its full key; the
                // platform engine handles them identically on both targets, and a key
                // whose `peer` did not open the session simply misses (owner rejection).
                Effect::Write { key, bytes } => {
                    self.transport.write(&key, bytes).await;
                }
                Effect::Shutdown { key } => {
                    self.transport.shutdown(&key).await;
                }
                Effect::Close { key } => {
                    self.transport.close(&key);
                }
                Effect::Listen {
                    local_addr,
                    peer,
                    service,
                    namespace,
                    kind,
                } => {
                    #[cfg(feature = "node")]
                    self.transport
                        .clone()
                        .listen(
                            self.processor.clone(),
                            local_addr,
                            peer,
                            service,
                            namespace,
                            kind,
                        )
                        .await;
                    #[cfg(feature = "browser")]
                    {
                        let _ = (local_addr, peer, service, namespace, kind);
                        tracing::warn!("transport Listen is unsupported on browser");
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

/// Erased, runtime-facing handler. Implemented once, generically, by `Runner`.
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
    compute: ComputeServices,
}

impl Extensions {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The shared [`ComputeServices`], handed to each [`Interpreter`] so the jobs a
    /// protocol registers are visible when its [`Effect::Compute`]s run.
    pub fn computes(&self) -> ComputeServices {
        self.compute.clone()
    }

    /// Register an impure [`ComputeFn`] for `namespace` (see [`Effect::Compute`]).
    pub fn register_compute(&self, namespace: impl Into<String>, job: ComputeFn) -> Result<()> {
        self.compute.register(namespace, job)
    }

    /// Register a pure [`Protocol`] under its namespace (wrapped in a `Runner`).
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
    /// `MAX_FIXPOINT_STEPS` is hit. This is the bounded least fixpoint of
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
