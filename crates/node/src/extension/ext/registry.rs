//! Router + capability core.
//!
//! [`Extensions`] registers `(Protocol, Interpret)` pairs by namespace. Each interpreter is
//! handed a namespace-scoped [`Scope`] (overlay `send` / `did` / self-`inject`, confined to its
//! own namespace) — *not* the router-internal `Core`. `Core` is the crate-private capability
//! that also routes an inbound [`Envelope`] to its protocol and drives the bounded re-injection
//! fixpoint; the registry stays uniform (everything erased to the internal `Handler`) while each
//! extension's shell is its own. Extension authors see only `Protocol` / `Interpret` / `Scope` /
//! `Transition` / `Extensions`.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

use bytes::Bytes;
use futures::lock::Mutex as AsyncMutex;
use rings_core::dht::Did;

use super::Ctx;
use super::Envelope;
use super::Inbound;
use super::Interpret;
use super::MaybeSend;
use super::Protocol;
use super::Reject;
use super::Transition;
use super::Wire;
use crate::error::Error;
use crate::error::Result;
use crate::processor::Processor;

/// Upper bound on re-injection iterations per inbound message, so a misbehaving
/// protocol/effect cycle cannot diverge.
const MAX_FIXPOINT_STEPS: u32 = 1024;

/// Type-erased handler stored in the registry: native is `Send + Sync`, browser not.
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
pub(crate) type DynHandler = dyn Handler + Send + Sync;
/// Type-erased handler stored in the registry.
#[cfg(all(feature = "browser", target_family = "wasm"))]
pub(crate) type DynHandler = dyn Handler;

type HandlerMap = RwLock<HashMap<String, Arc<DynHandler>>>;

/// Erased, runtime-facing handler — the router-internal ABI. Implemented once, generically, by
/// `Runner`; protocol authors never name it (they write `Protocol` + `Interpret`).
#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait::async_trait(?Send))]
#[cfg_attr(
    not(all(feature = "browser", target_family = "wasm")),
    async_trait::async_trait
)]
pub(crate) trait Handler {
    /// Decode → step (pure, committed) → run the protocol's effects, returning re-injected
    /// messages. `handle : (from, payload) → IO [Inbound]`.
    async fn handle(&self, core: &Core, from: Did, payload: Bytes) -> Result<Vec<Inbound>>;
}

/// The router-internal capability. Cloneable and `'static` so a long-running engine task can
/// keep a copy and feed events back via [`inject`](Core::inject). Not handed to extension
/// shells — they get a namespace-scoped [`Scope`] instead.
#[derive(Clone)]
pub(crate) struct Core {
    processor: Arc<Processor>,
    handlers: Arc<HandlerMap>,
}

impl Core {
    /// This node's DID.
    pub fn did(&self) -> Did {
        self.processor.did()
    }

    /// Put a message on the overlay to `to` under `namespace`.
    pub async fn send(&self, to: Did, namespace: &str, payload: Bytes) -> Result<()> {
        let envelope = Envelope::new(namespace, payload);
        self.processor.send_envelope(to, &envelope).await?;
        Ok(())
    }

    /// Re-enter the router with a *self*-addressed message (`from = this node`): a locally
    /// injected command, or an engine task feeding a lifecycle event back to its protocol.
    pub async fn inject(&self, namespace: &str, payload: Bytes) -> Result<()> {
        self.dispatch(self.did(), Envelope::new(namespace, payload))
            .await
    }

    /// Route an inbound [`Envelope`] to its protocol and drive the bounded re-injection
    /// fixpoint. Unknown namespaces are logged and dropped (non-fatal).
    ///
    /// This is the **authenticated ingress** capability: the caller chooses `from`, so a
    /// protocol's `decode` will attribute the resulting event to that DID (for the relay, a
    /// `from != me` envelope becomes a peer `Frame`). It is therefore `pub(crate)` — only the
    /// router path may call it, and only [`Backend`](crate::extension::Backend) does, with
    /// `from` taken from the message's verified signer. Extension code reaches the router only
    /// through [`inject`](Core::inject) (self-addressed, `from = self.did()`); it can never
    /// forge a remote `from`.
    pub(crate) async fn dispatch(&self, from: Did, envelope: Envelope) -> Result<()> {
        let mut queue: VecDeque<Inbound> = VecDeque::new();
        queue.push_back(Inbound {
            namespace: envelope.namespace,
            from,
            payload: envelope.payload,
        });

        let mut budget = MAX_FIXPOINT_STEPS;
        while let Some(Inbound {
            namespace,
            from,
            payload,
        }) = queue.pop_front()
        {
            if budget == 0 {
                return Err(Error::ExtensionError(format!(
                    "fixpoint budget ({MAX_FIXPOINT_STEPS}) exhausted; last namespace {namespace:?}"
                )));
            }
            budget -= 1;

            match self.handler(namespace.as_str()) {
                Some(handler) => queue.extend(handler.handle(self, from, payload).await?),
                None => tracing::debug!(
                    "no protocol registered for namespace {:?}, dropping",
                    namespace
                ),
            }
        }
        Ok(())
    }

    fn handler(&self, namespace: &str) -> Option<Arc<DynHandler>> {
        self.handlers.read().ok()?.get(namespace).map(Arc::clone)
    }
}

/// A **namespace-scoped** capability handed to an [`Interpret`] shell — the effectful
/// counterpart of the pure side's read-only [`Ctx`]. Every action is confined to the
/// interpreter's own namespace: it may `send` to peers and self-`inject` only there, and it
/// can neither reach another namespace nor forge a remote `from`. This is what keeps the
/// capability honest: an extension shell cannot use the router as a generic
/// inject-any-namespace bus (e.g. manufacture another extension's lifecycle events). Cloneable
/// and `'static`, so a long-running engine task can keep a copy.
#[derive(Clone)]
pub struct Scope {
    core: Core,
    namespace: String,
}

impl Scope {
    /// Confine `core` to `namespace`.
    pub(crate) fn new(core: Core, namespace: String) -> Self {
        Self { core, namespace }
    }

    /// This node's DID.
    pub fn did(&self) -> Did {
        self.core.did()
    }

    /// The namespace this scope is confined to.
    pub fn namespace(&self) -> &str {
        self.namespace.as_str()
    }

    /// Put a message on the overlay to `to`, under this interpreter's own namespace.
    pub async fn send(&self, to: Did, payload: Bytes) -> Result<()> {
        self.core.send(to, self.namespace.as_str(), payload).await
    }

    /// Self-inject `payload` into this interpreter's **own** namespace (`from = this node`).
    ///
    /// `pub(crate)`: this is the **long-lived lifecycle sink** for an extension's own engine
    /// (e.g. the relay's spawned socket tasks reporting `Accepted`/`Untrack` later), and it
    /// starts a **fresh** [`dispatch`](Core::dispatch) fixpoint with its own
    /// `MAX_FIXPOINT_STEPS` budget — it is *not* part of the bounded re-injection fixpoint that
    /// drives a single inbound. The synchronous per-effect feedback path is the `Vec<Bytes>`
    /// returned from [`Interpret::run`], which the router re-injects within the current budget.
    /// A third-party shell therefore gets only that bounded return path, never this re-entrant
    /// sink, so it cannot recurse `inject` to escape the budget.
    pub(crate) async fn inject(&self, payload: Bytes) -> Result<()> {
        self.core.inject(self.namespace.as_str(), payload).await
    }
}

/// Adapter binding a pure [`Protocol`] to its [`Interpret`] shell and owned state; erased to
/// [`Handler`]. Protocol authors never write this.
struct Runner<P: Protocol, I> {
    protocol: P,
    interpret: I,
    state: Mutex<P::State>,
    transition_gate: AsyncMutex<()>,
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait::async_trait(?Send))]
#[cfg_attr(
    not(all(feature = "browser", target_family = "wasm")),
    async_trait::async_trait
)]
impl<P, I> Handler for Runner<P, I>
where
    P: Protocol + MaybeSend + 'static,
    P::State: MaybeSend + 'static,
    P::Effect: MaybeSend,
    I: Interpret<Effect = P::Effect> + MaybeSend + 'static,
{
    async fn handle(&self, core: &Core, from: Did, payload: Bytes) -> Result<Vec<Inbound>> {
        // The transition gate establishes the protocol's linearization order before either the
        // decode boundary, state commit, or effects can begin. Holding it through interpretation
        // preserves: commit(A) < commit(B) => effects(A) complete before effects(B) begin.
        // The state mutex itself remains synchronous and never crosses an await.
        let _transition_turn = self.transition_gate.lock().await;

        // Boundary: decode raw bytes to a typed event. An undecodable/foreign message is an
        // explicit drop here, not a silent `Transition::pure` deep in `step`.
        let event = match self.protocol.decode(Wire {
            from,
            me: core.did(),
            payload: payload.as_ref(),
        }) {
            Ok(event) => event,
            Err(Reject(why)) => {
                tracing::debug!("drop on {}: {why}", self.protocol.namespace());
                return Ok(Vec::new());
            }
        };

        // Pure region: a brief *synchronous* critical section — read state, run `step`,
        // commit next state. No `.await` inside, so the std `Mutex` is correct and the state
        // fold stays serial per protocol (state-machine semantics, not a limitation;
        // different protocols and all effects below run concurrently). The commit is the
        // logical transition point; effect failures that matter come back as events.
        let effects = {
            let mut guard = self.state.lock().map_err(|_| Error::Lock)?;
            let Transition { state, effects } = self.protocol.step(
                Ctx {
                    did: core.did(),
                    state: guard.deref(),
                },
                event,
            );
            *guard = state;
            effects
        };

        // Impure region: the state lock is released, while the transition gate keeps this
        // protocol's effect trace in the same order as its committed state trace.
        // Run the protocol's own effects via its interpreter,
        // handing it only a namespace-scoped capability. Each payload it returns is re-injected
        // into *this* namespace with `from = this node` — the router fixes the provenance, so a
        // shell cannot forge a target namespace or a remote `from`.
        let namespace = self.protocol.namespace().to_string();
        let scope = Scope::new(core.clone(), namespace.clone());
        let mut reinjected = Vec::new();
        for effect in effects {
            for payload in self.interpret.run(&scope, effect).await? {
                reinjected.push(Inbound {
                    namespace: namespace.clone(),
                    from: core.did(),
                    payload,
                });
            }
        }
        Ok(reinjected)
    }
}

/// Registry of `(Protocol, Interpret)` pairs by namespace, plus the router-internal `Core`.
/// Cheaply cloneable and shared (interior mutability) so the
/// [`Provider`](crate::provider::Provider) and the inbound callback see the same table.
#[derive(Clone)]
pub struct Extensions {
    core: Core,
}

impl Extensions {
    /// Empty registry over a processor (the source of overlay `send` / `did`).
    pub fn new(processor: Arc<Processor>) -> Self {
        Self {
            core: Core {
                processor,
                handlers: Arc::new(RwLock::new(HashMap::new())),
            },
        }
    }

    /// The capability handle (overlay `send` / `did` / self-addressed `inject`). `pub(crate)`:
    /// public holders of an `Extensions` get **registration only** (`register` / `replace` /
    /// `contains` / `register_many`), never a raw [`Core`]. An extension's local injection is
    /// exposed through its own typed handle (e.g. `RelayHandle`), so application code cannot use
    /// a generic inject-any-namespace bus to forge engine-lifecycle commands like the relay's
    /// `Accepted` / `Untrack`.
    pub(crate) fn core(&self) -> Core {
        self.core.clone()
    }

    /// Register a protocol together with its interpreter under the protocol's namespace.
    /// Errors if the namespace is already taken — use [`replace`](Extensions::replace) for
    /// intentional replacement (no more silent overwrite).
    pub fn register<P, I>(&self, protocol: P, interpret: I) -> Result<()>
    where
        P: Protocol + MaybeSend + 'static,
        P::State: MaybeSend + 'static,
        P::Effect: MaybeSend,
        I: Interpret<Effect = P::Effect> + MaybeSend + 'static,
    {
        self.insert(protocol, interpret, false)
    }

    /// Like [`register`](Extensions::register) but replaces an existing protocol on the same
    /// namespace instead of erroring. For deliberate hot-swaps.
    pub fn replace<P, I>(&self, protocol: P, interpret: I) -> Result<()>
    where
        P: Protocol + MaybeSend + 'static,
        P::State: MaybeSend + 'static,
        P::Effect: MaybeSend,
        I: Interpret<Effect = P::Effect> + MaybeSend + 'static,
    {
        self.insert(protocol, interpret, true)
    }

    /// Register several protocols **atomically**: build every runner, then under a single write
    /// lock verify that none of their namespaces is taken (by an existing registration or by a
    /// duplicate within the batch) and insert them all — or change nothing and return `Err`. The
    /// pairs share the type `P`/`I` (e.g. the relay's TCP + UDP `Relay<T>` instances), so a
    /// partial install can never leave one namespace claimed while the caller gets no handle.
    pub fn register_many<P, I>(&self, items: Vec<(P, I)>) -> Result<()>
    where
        P: Protocol + MaybeSend + 'static,
        P::State: MaybeSend + 'static,
        P::Effect: MaybeSend,
        I: Interpret<Effect = P::Effect> + MaybeSend + 'static,
    {
        // Build (namespace, runner) outside the lock.
        let prepared: Vec<(String, Arc<DynHandler>)> = items
            .into_iter()
            .map(|(protocol, interpret)| {
                let namespace = protocol.namespace().to_string();
                let state = Mutex::new(protocol.init());
                let runner: Arc<DynHandler> = Arc::new(Runner {
                    protocol,
                    interpret,
                    state,
                    transition_gate: AsyncMutex::new(()),
                });
                (namespace, runner)
            })
            .collect();

        let mut handlers = self.core.handlers.write().map_err(|_| Error::Lock)?;
        // Check-all (existing table + intra-batch duplicates) before mutating anything.
        for (index, (namespace, _)) in prepared.iter().enumerate() {
            let duplicate_in_batch = prepared
                .iter()
                .take(index)
                .any(|(seen, _)| seen == namespace);
            if duplicate_in_batch || handlers.contains_key(namespace) {
                return Err(Error::ExtensionError(format!(
                    "namespace {namespace:?} is already registered"
                )));
            }
        }
        // All free: insert the whole batch.
        for (namespace, runner) in prepared {
            handlers.insert(namespace, runner);
        }
        Ok(())
    }

    fn insert<P, I>(&self, protocol: P, interpret: I, replace: bool) -> Result<()>
    where
        P: Protocol + MaybeSend + 'static,
        P::State: MaybeSend + 'static,
        P::Effect: MaybeSend,
        I: Interpret<Effect = P::Effect> + MaybeSend + 'static,
    {
        let namespace = protocol.namespace().to_string();
        let state = Mutex::new(protocol.init());
        let runner: Arc<DynHandler> = Arc::new(Runner {
            protocol,
            interpret,
            state,
            transition_gate: AsyncMutex::new(()),
        });
        let mut handlers = self.core.handlers.write().map_err(|_| Error::Lock)?;
        if !replace && handlers.contains_key(&namespace) {
            return Err(Error::ExtensionError(format!(
                "namespace {namespace:?} is already registered"
            )));
        }
        handlers.insert(namespace, runner);
        Ok(())
    }

    /// Whether a namespace is registered.
    pub fn contains(&self, namespace: &str) -> bool {
        self.core
            .handlers
            .read()
            .map(|h| h.contains_key(namespace))
            .unwrap_or(false)
    }

    /// Route a decoded envelope (inbound entry point). `pub(crate)`: the authenticated ingress
    /// belongs to the router path ([`Backend`](crate::extension::Backend)), not to public
    /// holders of an `Extensions` value (which get registration + a self-addressed
    /// [`Core`](Core::inject), never the ability to forge a remote `from`). See
    /// [`Core::dispatch`].
    pub(crate) async fn dispatch(&self, from: Did, envelope: Envelope) -> Result<()> {
        self.core.dispatch(from, envelope).await
    }
}

#[cfg(all(test, feature = "node"))]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use rings_core::ecc::SecretKey;
    use rings_core::session::SessionSk;
    use tokio::sync::Notify;

    use super::*;
    use crate::processor::ProcessorBuilder;
    use crate::processor::ProcessorConfig;

    struct OrderedProtocol;

    impl Protocol for OrderedProtocol {
        type State = u8;
        type Event = u8;
        type Effect = u8;

        fn namespace(&self) -> &str {
            "ordered-effects"
        }

        fn init(&self) -> Self::State {
            0
        }

        fn decode(&self, wire: Wire<'_>) -> std::result::Result<Self::Event, Reject> {
            wire.payload
                .first()
                .copied()
                .ok_or_else(|| Reject("missing effect value".to_string()))
        }

        fn step(
            &self,
            ctx: Ctx<'_, Self::State>,
            event: Self::Event,
        ) -> Transition<Self::State, Self::Effect> {
            Transition::with(ctx.state.saturating_add(1), vec![event])
        }
    }

    #[derive(Default)]
    struct BlockingOrderedInterpreter {
        first_effect_started: Notify,
        release_first_effect: Notify,
        observed: Mutex<Vec<u8>>,
    }

    #[async_trait]
    impl Interpret for Arc<BlockingOrderedInterpreter> {
        type Effect = u8;

        async fn run(&self, _scope: &Scope, effect: Self::Effect) -> Result<Vec<Bytes>> {
            if effect == 1 {
                self.first_effect_started.notify_one();
                self.release_first_effect.notified().await;
            }
            self.observed.lock().map_err(|_| Error::Lock)?.push(effect);
            Ok(Vec::new())
        }
    }

    fn extensions() -> Result<Extensions> {
        let session = SessionSk::new_with_seckey(&SecretKey::random())?;
        let config = ProcessorConfig::new(1, String::new(), session, 1);
        let processor = ProcessorBuilder::from_config(&config)?
            .advertise_presence(false)
            .build()?;
        Ok(Extensions::new(Arc::new(processor)))
    }

    #[tokio::test]
    async fn committed_transitions_execute_effects_in_commit_order() -> Result<()> {
        // Invariant: while A's effect is blocked, B cannot commit or emit; releasing A
        // produces the unique effect trace [A, B] for the protocol's state-transition order.
        let extensions = extensions()?;
        let interpreter = Arc::new(BlockingOrderedInterpreter::default());
        extensions.register(OrderedProtocol, Arc::clone(&interpreter))?;
        let from = extensions.core().did();

        let first_extensions = extensions.clone();
        let first = tokio::spawn(async move {
            first_extensions
                .dispatch(
                    from,
                    Envelope::new("ordered-effects", Bytes::from_static(&[1])),
                )
                .await
        });
        interpreter.first_effect_started.notified().await;

        let second_extensions = extensions.clone();
        let second = tokio::spawn(async move {
            second_extensions
                .dispatch(
                    from,
                    Envelope::new("ordered-effects", Bytes::from_static(&[2])),
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        assert!(interpreter
            .observed
            .lock()
            .map_err(|_| Error::Lock)?
            .is_empty());

        interpreter.release_first_effect.notify_one();
        first
            .await
            .map_err(|error| Error::ExtensionError(error.to_string()))??;
        second
            .await
            .map_err(|error| Error::ExtensionError(error.to_string()))??;
        assert_eq!(
            *interpreter.observed.lock().map_err(|_| Error::Lock)?,
            vec![1, 2]
        );
        Ok(())
    }
}
