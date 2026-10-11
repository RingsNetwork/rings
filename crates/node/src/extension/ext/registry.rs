//! Router + capability core.
//!
//! [`Extensions`] registers `(Protocol, Interpret)` pairs by namespace. Each interpreter is
//! handed a namespace-scoped [`Scope`] (overlay `send` / `did` / self-`inject`, confined to its
//! own namespace) — *not* the router-internal `Core`. Protocols that authenticate and select an
//! exact physical next hop may additionally use crate-private `send_direct`; it preserves the
//! namespace envelope while bypassing Chord route selection. `Core` is the crate-private capability
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
use rings_runtime::MaybeSendSync;

use super::delegation;
use super::Ctx;
use super::Envelope;
use super::Interpret;
use super::Protocol;
use super::Reject;
use super::Transition;
use super::Wire;
use crate::error::Error;
use crate::error::Result;
use crate::processor::Processor;
use crate::sync_lock::lock;

/// Upper bound on synchronous feedback iterations within one protocol turn.
const MAX_FIXPOINT_STEPS: u32 = 1024;

/// Type-erased handler stored in the registry: native is `Send + Sync`, browser not.
pub(crate) type DynHandler = rings_runtime::maybe_send_sync!(dyn Handler);

type HandlerMap = RwLock<HashMap<String, Arc<DynHandler>>>;

/// Erased, runtime-facing handler — the router-internal ABI. Implemented once, generically, by
/// `Runner`; protocol authors never name it (they write `Protocol` + `Interpret`).
#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
pub(crate) trait Handler {
    /// Decode → step (pure, committed) → run the protocol's effects and synchronous
    /// feedback. `handle : (from, payload) → IO ()`.
    async fn handle(&self, core: &Core, from: Did, payload: Bytes) -> Result<()>;
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

    #[cfg(rings_native)]
    pub(crate) fn onion_exit_epoch(&self) -> crate::onion::OnionExitEpoch {
        self.processor.onion_exit_epoch()
    }

    /// Put a message on the overlay to `to` under `namespace`.
    pub async fn send(&self, to: Did, namespace: &str, payload: Bytes) -> Result<()> {
        let envelope = Envelope::new(namespace, payload);
        self.processor.send_envelope(to, &envelope).await?;
        Ok(())
    }

    /// Put a message on an already-connected transport edge without Chord route selection.
    async fn send_direct(&self, to: Did, namespace: &str, payload: Bytes) -> Result<()> {
        let envelope = Envelope::new(namespace, payload);
        self.processor.send_direct_envelope(to, &envelope).await?;
        Ok(())
    }

    /// Re-enter the router with a *self*-addressed message (`from = this node`): a locally
    /// injected command, or an engine task feeding a lifecycle event back to its protocol.
    /// Only the native relay engine has such tasks.
    #[cfg(rings_native)]
    pub async fn inject(&self, namespace: &str, payload: Bytes) -> Result<()> {
        self.dispatch(self.did(), Envelope::new(namespace, payload))
            .await
    }

    /// Route one inbound [`Envelope`] to its protocol. Unknown namespaces are logged and
    /// dropped (non-fatal). Same-turn feedback is reduced inside the selected handler; later
    /// lifecycle injection starts a new dispatch.
    ///
    /// This is the **authenticated ingress** capability: the caller chooses `from`, so a
    /// protocol's `decode` will attribute the resulting event to that DID (for the relay, a
    /// `from != me` envelope becomes a peer `Frame`). It is therefore `pub(crate)` — only the
    /// router path may call it, and only [`Backend`](crate::extension::Backend) does, with
    /// `from` taken from the message's verified signer. Extension code reaches the router only
    /// through [`inject`](Core::inject) (self-addressed, `from = self.did()`); it can never
    /// forge a remote `from`.
    pub(crate) async fn dispatch(&self, from: Did, envelope: Envelope) -> Result<()> {
        let namespace = envelope.namespace;
        match self.handler(namespace.as_str()) {
            Some(handler) => handler.handle(self, from, envelope.payload).await?,
            None => tracing::debug!(
                "no protocol registered for namespace {:?}, dropping",
                namespace
            ),
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

    /// Send to an exact, already-connected peer under this scope's namespace.
    ///
    /// This is intentionally crate-private: only protocols that already authenticate their own
    /// hop selection, such as onion circuits, may bypass the overlay routing decision.
    pub(crate) async fn send_direct(&self, to: Did, payload: Bytes) -> Result<()> {
        self.core
            .send_direct(to, self.namespace.as_str(), payload)
            .await
    }

    /// Self-inject `payload` into this interpreter's **own** namespace (`from = this node`).
    ///
    /// `pub(crate)`: this is the **long-lived lifecycle sink** for an extension's own engine
    /// (e.g. the relay's spawned socket tasks reporting `Accepted`/`Untrack` later), and it
    /// starts a **fresh** [`dispatch`](Core::dispatch) fixpoint with its own
    /// `MAX_FIXPOINT_STEPS` budget — it is *not* part of the bounded feedback fixpoint that
    /// drives a single inbound. The synchronous per-effect feedback path is the `Vec<Bytes>`
    /// returned from [`Interpret::run`], which the runner reduces within its current ordered
    /// turn and budget.
    /// A third-party shell therefore gets only that bounded return path, never this re-entrant
    /// sink, so it cannot recurse `inject` to escape the budget.
    #[cfg(rings_native)]
    pub(crate) async fn inject(&self, payload: Bytes) -> Result<()> {
        self.core.inject(self.namespace.as_str(), payload).await
    }
}

/// Capability available while an interpreter applies one committed effect.
///
/// It can send overlay messages and return synchronous feedback, but cannot re-enter its own
/// reducer. Long-lived engines receive a [`Scope`] explicitly through the crate-private
/// `EffectScope::lifecycle` handoff, making ownership visible at the effect boundary.
pub struct EffectScope {
    scope: Scope,
}

impl EffectScope {
    pub(crate) fn new(scope: Scope) -> Self {
        Self { scope }
    }

    /// This node's DID.
    pub fn did(&self) -> Did {
        self.scope.did()
    }

    /// The namespace this effect is confined to.
    pub fn namespace(&self) -> &str {
        self.scope.namespace()
    }

    /// Put a message on the overlay under this effect's namespace.
    pub async fn send(&self, to: Did, payload: Bytes) -> Result<()> {
        self.scope.send(to, payload).await
    }

    /// Hand the lifecycle capability to an explicitly long-lived engine task.
    ///
    /// The interpreter itself must return same-turn feedback from [`Interpret::run`], rather
    /// than await [`Scope::inject`] while the ordered effect turn is active.
    pub(crate) fn lifecycle(&self) -> Scope {
        self.scope.clone()
    }
}

/// Adapter binding a pure [`Protocol`] to its [`Interpret`] shell and owned state; erased to
/// [`Handler`]. Protocol authors never write this.
struct Runner<P: Protocol, I> {
    protocol: P,
    interpret: I,
    state: Mutex<P::State>,
    transition_gate: AsyncMutex<()>,
    #[cfg(all(test, rings_native))]
    after_decode_for_test: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(all(test, rings_native))]
    after_commit_for_test: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(all(test, rings_native))]
    before_gate_wait_for_test: Option<Arc<dyn Fn(bool) + Send + Sync>>,
}

#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
impl<P, I> Handler for Runner<P, I>
where
    P: Protocol + MaybeSendSync + 'static,
    P::State: MaybeSendSync + 'static,
    P::Effect: MaybeSendSync,
    I: Interpret<Effect = P::Effect> + MaybeSendSync + 'static,
{
    async fn handle(&self, core: &Core, from: Did, payload: Bytes) -> Result<()> {
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
                return Ok(());
            }
        };

        #[cfg(all(test, rings_native))]
        if let Some(observe) = self.after_decode_for_test.as_ref() {
            observe();
        }

        // The transition gate establishes the protocol's linearization order for state commit
        // and its resulting effect trace. Decode remains outside that order: it has no state or
        // effects by contract. Holding the gate through interpretation preserves:
        // commit(A) < commit(B) => applying A's effects ends before applying B's effects begins.
        // The state mutex itself remains synchronous and never crosses an await.
        #[cfg(all(test, rings_native))]
        if let Some(observe) = self.before_gate_wait_for_test.as_ref() {
            // Witness the real synchronization boundary: false means this task could acquire
            // the gate immediately; true means another transition owns it at this exact point.
            observe(self.transition_gate.try_lock().is_none());
        }
        let _transition_turn = self.transition_gate.lock().await;

        // Impure region: the state lock is released, while the transition gate keeps this
        // protocol's effect trace and synchronous feedback fixpoint in commit order. Returned
        // payloads are reduced before this turn releases the gate, so a later inbound cannot
        // observe state that predates an effect's own feedback.
        let namespace = self.protocol.namespace().to_string();
        let scope = EffectScope::new(Scope::new(core.clone(), namespace.clone()));
        let mut feedback = VecDeque::new();
        feedback.push_back(event);
        let mut feedback_budget = MAX_FIXPOINT_STEPS;
        while let Some(event) = feedback.pop_front() {
            if feedback_budget == 0 {
                return Err(Error::ExtensionError(format!(
                    "feedback fixpoint budget ({MAX_FIXPOINT_STEPS}) exhausted on {namespace:?}"
                )));
            }
            feedback_budget -= 1;

            // Pure region: a brief synchronous state fold. No await crosses `state`; the
            // feedback queue makes every same-turn effect result part of this fold before a
            // competing inbound can begin its own transition.
            let effects = {
                let mut guard = lock(&self.state)?;
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

            #[cfg(all(test, rings_native))]
            if let Some(observe) = self.after_commit_for_test.as_ref() {
                observe();
            }

            for effect in effects {
                for payload in self.interpret.run(&scope, effect).await? {
                    match self.protocol.decode(Wire {
                        from: core.did(),
                        me: core.did(),
                        payload: payload.as_ref(),
                    }) {
                        Ok(event) => feedback.push_back(event),
                        Err(Reject(why)) => {
                            tracing::debug!("drop feedback on {}: {why}", self.protocol.namespace())
                        }
                    }
                }
            }
        }
        Ok(())
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
        P: Protocol + MaybeSendSync + 'static,
        P::State: MaybeSendSync + 'static,
        P::Effect: MaybeSendSync,
        I: Interpret<Effect = P::Effect> + MaybeSendSync + 'static,
    {
        self.insert(protocol, interpret, false)
    }

    /// Like [`register`](Extensions::register) but replaces an existing protocol on the same
    /// namespace instead of erroring. For deliberate hot-swaps.
    pub fn replace<P, I>(&self, protocol: P, interpret: I) -> Result<()>
    where
        P: Protocol + MaybeSendSync + 'static,
        P::State: MaybeSendSync + 'static,
        P::Effect: MaybeSendSync,
        I: Interpret<Effect = P::Effect> + MaybeSendSync + 'static,
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
        P: Protocol + MaybeSendSync + 'static,
        P::State: MaybeSendSync + 'static,
        P::Effect: MaybeSendSync,
        I: Interpret<Effect = P::Effect> + MaybeSendSync + 'static,
    {
        // Build (namespace, runner) outside the lock.
        let prepared: Vec<(String, bool, Arc<DynHandler>)> = items
            .into_iter()
            .map(|(protocol, interpret)| {
                let namespace = protocol.namespace().to_string();
                let delegates = protocol.delegates_admission();
                let state = Mutex::new(protocol.init());
                let runner: Arc<DynHandler> = Arc::new(Runner {
                    protocol,
                    interpret,
                    state,
                    transition_gate: AsyncMutex::new(()),
                    #[cfg(all(test, rings_native))]
                    after_decode_for_test: None,
                    #[cfg(all(test, rings_native))]
                    after_commit_for_test: None,
                    #[cfg(all(test, rings_native))]
                    before_gate_wait_for_test: None,
                });
                (namespace, delegates, runner)
            })
            .collect();

        let mut handlers = self.core.handlers.write().map_err(|_| Error::Lock)?;
        // Check-all (existing table + intra-batch duplicates) before mutating anything.
        for (index, (namespace, _, _)) in prepared.iter().enumerate() {
            let duplicate_in_batch = prepared
                .iter()
                .take(index)
                .any(|(seen, _, _)| seen == namespace);
            if duplicate_in_batch || handlers.contains_key(namespace) {
                return Err(Error::ExtensionError(format!(
                    "namespace {namespace:?} is already registered"
                )));
            }
        }
        // All free: publish the declarations and insert the batch under the same write lock.
        delegation::publish(
            &self.core.processor.swarm,
            prepared
                .iter()
                .map(|(namespace, delegates, _)| (namespace.as_str(), *delegates)),
        )?;
        for (namespace, _, runner) in prepared {
            handlers.insert(namespace, runner);
        }
        Ok(())
    }

    fn insert<P, I>(&self, protocol: P, interpret: I, replace: bool) -> Result<()>
    where
        P: Protocol + MaybeSendSync + 'static,
        P::State: MaybeSendSync + 'static,
        P::Effect: MaybeSendSync,
        I: Interpret<Effect = P::Effect> + MaybeSendSync + 'static,
    {
        let namespace = protocol.namespace().to_string();
        let delegates = protocol.delegates_admission();
        let state = Mutex::new(protocol.init());
        let runner: Arc<DynHandler> = Arc::new(Runner {
            protocol,
            interpret,
            state,
            transition_gate: AsyncMutex::new(()),
            #[cfg(all(test, rings_native))]
            after_decode_for_test: None,
            #[cfg(all(test, rings_native))]
            after_commit_for_test: None,
            #[cfg(all(test, rings_native))]
            before_gate_wait_for_test: None,
        });
        let mut handlers = self.core.handlers.write().map_err(|_| Error::Lock)?;
        if !replace && handlers.contains_key(&namespace) {
            return Err(Error::ExtensionError(format!(
                "namespace {namespace:?} is already registered"
            )));
        }
        delegation::publish(&self.core.processor.swarm, [(
            namespace.as_str(),
            delegates,
        )])?;
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

    /// Whether an encoded envelope's namespace delegates its admission, as core reads it; see
    /// [`Protocol::delegates_admission`].
    #[cfg(all(test, rings_native))]
    pub(crate) fn delegates_admission(&self, envelope: &[u8]) -> bool {
        self.core.processor.swarm.delegates_admission(envelope)
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

#[cfg(all(test, rings_native))]
mod tests;
