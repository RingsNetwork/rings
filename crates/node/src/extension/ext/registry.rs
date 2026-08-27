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
use crate::sync_lock::lock;

/// Upper bound on re-injection iterations per inbound message, so a misbehaving
/// protocol/effect cycle cannot diverge.
const MAX_FIXPOINT_STEPS: u32 = 1024;

/// Type-erased handler stored in the registry: native is `Send + Sync`, browser not.
#[cfg(rings_native)]
pub(crate) type DynHandler = dyn Handler + Send + Sync;
/// Type-erased handler stored in the registry.
#[cfg(rings_browser)]
pub(crate) type DynHandler = dyn Handler;

type HandlerMap = RwLock<HashMap<String, Arc<DynHandler>>>;

/// Erased, runtime-facing handler — the router-internal ABI. Implemented once, generically, by
/// `Runner`; protocol authors never name it (they write `Protocol` + `Interpret`).
#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
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
    /// `MAX_FIXPOINT_STEPS` budget — it is *not* part of the bounded feedback fixpoint that
    /// drives a single inbound. The synchronous per-effect feedback path is the `Vec<Bytes>`
    /// returned from [`Interpret::run`], which the runner reduces within its current ordered
    /// turn and budget.
    /// A third-party shell therefore gets only that bounded return path, never this re-entrant
    /// sink, so it cannot recurse `inject` to escape the budget.
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
    P: Protocol + MaybeSend + 'static,
    P::State: MaybeSend + 'static,
    P::Effect: MaybeSend,
    I: Interpret<Effect = P::Effect> + MaybeSend + 'static,
{
    async fn handle(&self, core: &Core, from: Did, payload: Bytes) -> Result<Vec<Inbound>> {
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
        Ok(Vec::new())
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
        let prepared: Vec<(String, Arc<DynHandler>, Vec<&'static str>)> = items
            .into_iter()
            .map(|(protocol, interpret)| {
                let capabilities = protocol.capabilities().to_vec();
                let namespace = protocol.namespace().to_string();
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
                (namespace, runner, capabilities)
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
        self.core.processor.add_online_node_capabilities(
            prepared
                .iter()
                .flat_map(|(_, _, capabilities)| capabilities.iter().copied()),
        )?;
        // All free: insert the whole batch.
        for (namespace, runner, _) in prepared {
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
        let capabilities = protocol.capabilities();
        let namespace = protocol.namespace().to_string();
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
        self.core
            .processor
            .add_online_node_capabilities(capabilities.iter().copied())?;
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

#[cfg(all(test, rings_native))]
mod tests {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use rings_core::ecc::SecretKey;
    use rings_core::session::SessionSk;
    use tokio::sync::Notify;

    use super::*;
    use crate::extension::protocols::relay::ControlSendTestHook;
    use crate::extension::protocols::relay::NativeRelay;
    use crate::extension::protocols::relay::Relay;
    use crate::extension::protocols::relay::RelayCommand;
    use crate::extension::protocols::relay::RelayEffect;
    use crate::extension::protocols::relay::TCP;
    use crate::extension::transport::engine::TransportSessions;
    use crate::extension::transport::Frame;
    use crate::extension::transport::Initiator;
    use crate::extension::transport::SessionId;
    use crate::extension::transport::SessionKey;
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
            let event = wire
                .payload
                .first()
                .copied()
                .ok_or_else(|| Reject("missing effect value".to_string()))?;
            Ok(event)
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

        async fn run(&self, _scope: &EffectScope, effect: Self::Effect) -> Result<Vec<Bytes>> {
            if effect == 1 {
                self.first_effect_started.notify_one();
                self.release_first_effect.notified().await;
            }
            lock(&self.observed)?.push(effect);
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct RelayFeedbackInterpreter {
        first_effect_started: Notify,
        release_first_effect: Notify,
        first_connect_seen: Mutex<bool>,
        observed_connects: Mutex<Vec<SessionId>>,
    }

    #[async_trait]
    impl Interpret for Arc<RelayFeedbackInterpreter> {
        type Effect = RelayEffect<SocketAddr>;

        async fn run(&self, _scope: &EffectScope, effect: Self::Effect) -> Result<Vec<Bytes>> {
            match effect {
                RelayEffect::Connect { key, .. } => {
                    let first_connect = {
                        let mut seen = lock(&self.first_connect_seen)?;
                        let first_connect = !*seen;
                        *seen = true;
                        first_connect
                    };
                    if first_connect {
                        self.first_effect_started.notify_one();
                        self.release_first_effect.notified().await;
                        let feedback = RelayCommand::<SocketAddr>::Untrack {
                            peer: key.peer,
                            session: key.session,
                            initiator: key.initiator,
                        };
                        return rings_codec::serialize(&feedback)
                            .map(Bytes::from)
                            .map(|payload| vec![payload])
                            .map_err(|_| Error::EncodeError);
                    }
                    lock(&self.observed_connects)?.push(key.session);
                    Ok(Vec::new())
                }
                _ => Ok(Vec::new()),
            }
        }
    }

    #[derive(Default)]
    struct FailingOrderedInterpreter {
        observed: Mutex<Vec<u8>>,
    }

    #[async_trait]
    impl Interpret for Arc<FailingOrderedInterpreter> {
        type Effect = u8;

        async fn run(&self, _scope: &EffectScope, effect: Self::Effect) -> Result<Vec<Bytes>> {
            lock(&self.observed)?.push(effect);
            if effect == 1 {
                return Err(Error::ExtensionError(
                    "intentional effect failure".to_string(),
                ));
            }
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
    async fn test_committed_transitions_execute_effects_in_commit_order() -> Result<()> {
        // Invariant: while A's effect is blocked, B cannot commit or emit; releasing A
        // produces the unique effect trace [A, B] for the protocol's state-transition order.
        let extensions = extensions()?;
        let interpreter = Arc::new(BlockingOrderedInterpreter::default());
        let gate_wait = Arc::new(Notify::new());
        let gate_contention = Arc::new(Mutex::new(Vec::new()));
        let gate_observer = {
            let gate_wait = Arc::clone(&gate_wait);
            let gate_contention = Arc::clone(&gate_contention);
            Arc::new(move |contended| {
                gate_contention
                    .lock()
                    .expect("test gate witness lock")
                    .push(contended);
                gate_wait.notify_one();
            }) as Arc<dyn Fn(bool) + Send + Sync>
        };
        let committed = Arc::new(Mutex::new(0_u8));
        let commit_observer = {
            let committed = Arc::clone(&committed);
            Arc::new(move || {
                *committed.lock().expect("test commit witness lock") += 1;
            }) as Arc<dyn Fn() + Send + Sync>
        };
        let runner: Arc<DynHandler> = Arc::new(Runner {
            protocol: OrderedProtocol,
            interpret: Arc::clone(&interpreter),
            state: Mutex::new(0),
            transition_gate: AsyncMutex::new(()),
            after_decode_for_test: None,
            after_commit_for_test: Some(commit_observer),
            before_gate_wait_for_test: Some(gate_observer),
        });
        extensions
            .core
            .handlers
            .write()
            .map_err(|_| Error::Lock)?
            .insert("ordered-effects".to_string(), runner);
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
        gate_wait.notified().await;

        let second_extensions = extensions.clone();
        let second = tokio::spawn(async move {
            second_extensions
                .dispatch(
                    from,
                    Envelope::new("ordered-effects", Bytes::from_static(&[2])),
                )
                .await
        });
        gate_wait.notified().await;
        assert_eq!(*lock(&gate_contention)?, vec![false, true]);
        assert!(!second.is_finished());
        assert!(lock(&interpreter.observed)?.is_empty());
        assert_eq!(*lock(&committed)?, 1);

        interpreter.release_first_effect.notify_one();
        first
            .await
            .map_err(|error| Error::ExtensionError(error.to_string()))??;
        second
            .await
            .map_err(|error| Error::ExtensionError(error.to_string()))??;
        assert_eq!(*lock(&committed)?, 2);
        assert_eq!(*lock(&interpreter.observed)?, vec![1, 2]);
        Ok(())
    }

    #[tokio::test]
    async fn test_failed_effect_releases_ordered_turn_for_later_transition() -> Result<()> {
        // Law: failure is an outcome of the committed transition, not a leaked gate. The next
        // transition can run after the failed application has ended.
        let extensions = extensions()?;
        let interpreter = Arc::new(FailingOrderedInterpreter::default());
        extensions.register(OrderedProtocol, Arc::clone(&interpreter))?;
        let from = extensions.core().did();

        let failed = extensions
            .dispatch(
                from,
                Envelope::new("ordered-effects", Bytes::from_static(&[1])),
            )
            .await;
        assert!(matches!(failed, Err(Error::ExtensionError(_))));
        extensions
            .dispatch(
                from,
                Envelope::new("ordered-effects", Bytes::from_static(&[2])),
            )
            .await?;

        assert_eq!(*lock(&interpreter.observed)?, vec![1, 2]);
        Ok(())
    }

    #[tokio::test]
    async fn test_returned_feedback_precedes_a_waiting_transition() -> Result<()> {
        // Invariant: the first inbound Open returns a real RelayCommand::Untrack feedback before
        // a duplicate Open may inspect relay state. Therefore the duplicate is accepted and emits
        // its own Connect; the old queue-after-gate implementation dropped it as a live duplicate.
        let extensions = extensions()?;
        let interpreter = Arc::new(RelayFeedbackInterpreter::default());
        let decoded = Arc::new(Notify::new());
        let observer = {
            let decoded = Arc::clone(&decoded);
            Arc::new(move || decoded.notify_one()) as Arc<dyn Fn() + Send + Sync>
        };
        let protocol = Relay::tcp(HashMap::from([(
            "web".to_string(),
            "127.0.0.1:80"
                .parse::<SocketAddr>()
                .map_err(|error| Error::ExtensionError(error.to_string()))?,
        )]));
        let state = protocol.init();
        let runner: Arc<DynHandler> = Arc::new(Runner {
            protocol,
            interpret: Arc::clone(&interpreter),
            state: Mutex::new(state),
            transition_gate: AsyncMutex::new(()),
            after_decode_for_test: Some(observer),
            after_commit_for_test: None,
            before_gate_wait_for_test: None,
        });
        extensions
            .core
            .handlers
            .write()
            .map_err(|_| Error::Lock)?
            .insert(TCP.to_string(), runner);
        let from: Did = SecretKey::random().address().into();
        let open = rings_codec::serialize(&Frame::Open {
            session: SessionId(0),
            service: "web".to_string(),
        })
        .map(Bytes::from)
        .map_err(|_| Error::EncodeError)?;
        let first_open = open.clone();

        let first_extensions = extensions.clone();
        let first = tokio::spawn(async move {
            first_extensions
                .dispatch(from, Envelope::new(TCP, first_open))
                .await
        });
        interpreter.first_effect_started.notified().await;
        decoded.notified().await;

        let second_extensions = extensions.clone();
        let second = tokio::spawn(async move {
            second_extensions
                .dispatch(from, Envelope::new(TCP, open))
                .await
        });
        decoded.notified().await;
        assert!(!second.is_finished());

        interpreter.release_first_effect.notify_one();
        let timeout = std::time::Duration::from_secs(1);
        tokio::time::timeout(timeout, first)
            .await
            .map_err(|_| Error::ExtensionError("first feedback turn timed out".to_string()))?
            .map_err(|error| Error::ExtensionError(error.to_string()))??;
        tokio::time::timeout(timeout, second)
            .await
            .map_err(|_| Error::ExtensionError("second feedback turn timed out".to_string()))?
            .map_err(|error| Error::ExtensionError(error.to_string()))??;
        assert_eq!(*lock(&interpreter.observed_connects)?, vec![SessionId(0)]);
        Ok(())
    }

    #[tokio::test]
    async fn test_missing_open_accepted_resource_returns_synchronous_untrack() -> Result<()> {
        let extensions = extensions()?;
        let effect_scope = EffectScope::new(Scope::new(extensions.core(), TCP.to_string()));
        let interpreter = NativeRelay::new(Arc::new(TransportSessions::new()));
        let peer: Did = SecretKey::random().address().into();
        let key = SessionKey::new(peer, TCP, SessionId(9), Initiator::Local);

        let feedback = interpreter
            .run(&effect_scope, RelayEffect::OpenAccepted {
                token: 77,
                key: key.clone(),
                service: "missing-pending-resource".to_string(),
            })
            .await?;

        assert_eq!(feedback.len(), 1);
        assert!(matches!(
            rings_codec::deserialize::<RelayCommand<SocketAddr>>(feedback[0].as_ref()),
            Ok(RelayCommand::Untrack {
                peer: actual_peer,
                session: SessionId(9),
                initiator: Initiator::Local,
            }) if actual_peer == peer
        ));
        Ok(())
    }

    #[tokio::test]
    async fn test_terminal_relay_control_effect_does_not_await_overlay_send() -> Result<()> {
        let extensions = extensions()?;
        let hook = Arc::new(ControlSendTestHook::default());
        let interpreter = Arc::new(NativeRelay::new_with_control_send_test_hook(
            Arc::new(TransportSessions::new()),
            Arc::clone(&hook),
        ));
        let peer: Did = SecretKey::random().address().into();
        let core = extensions.core();
        let application = tokio::spawn(async move {
            let effect_scope = EffectScope::new(Scope::new(core, TCP.to_string()));
            interpreter
                .run(&effect_scope, RelayEffect::SendClose {
                    to: peer,
                    session: SessionId(5),
                    from_opener: false,
                })
                .await
        });

        // Await a real outbox-worker suspension before observing completion. The interpreter
        // must already have returned; otherwise this join times out deterministically while the
        // hook remains held.
        tokio::time::timeout(std::time::Duration::from_secs(1), hook.wait_until_blocked())
            .await
            .map_err(|_| {
                Error::ExtensionError("control outbox did not reach test gate".to_string())
            })?;
        let applied = tokio::time::timeout(std::time::Duration::from_secs(1), application)
            .await
            .map_err(|_| {
                Error::ExtensionError("terminal control effect held the gate".to_string())
            })?
            .map_err(|error| Error::ExtensionError(error.to_string()))??;

        assert!(applied.is_empty());
        hook.release();
        Ok(())
    }

    #[tokio::test]
    async fn test_saturated_peer_control_lane_does_not_block_another_peer() -> Result<()> {
        let extensions = extensions()?;
        let hook = Arc::new(ControlSendTestHook::default());
        let interpreter = NativeRelay::new_with_control_send_test_hook(
            Arc::new(TransportSessions::new()),
            Arc::clone(&hook),
        );
        let blocked_peer: Did = SecretKey::random().address().into();
        let independent_peer: Did = SecretKey::random().address().into();
        let effect_scope = EffectScope::new(Scope::new(extensions.core(), TCP.to_string()));

        interpreter
            .run(&effect_scope, RelayEffect::SendClose {
                to: blocked_peer,
                session: SessionId(0),
                from_opener: false,
            })
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(1), hook.wait_until_blocked())
            .await
            .map_err(|_| {
                Error::ExtensionError("first peer control lane did not block".to_string())
            })?;

        let mut saturated = false;
        for session in 1..=8 {
            let result = interpreter
                .run(&effect_scope, RelayEffect::SendClose {
                    to: blocked_peer,
                    session: SessionId(session),
                    from_opener: false,
                })
                .await;
            if result.is_err() {
                saturated = true;
                break;
            }
        }
        assert!(saturated, "the blocked peer must have a finite lane budget");

        interpreter
            .run(&effect_scope, RelayEffect::SendClose {
                to: independent_peer,
                session: SessionId(9),
                from_opener: false,
            })
            .await?;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            hook.wait_until_completed(independent_peer),
        )
        .await
        .map_err(|_| {
            Error::ExtensionError("independent peer control lane was blocked".to_string())
        })??;

        hook.release();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            hook.wait_until_completed(blocked_peer),
        )
        .await
        .map_err(|_| Error::ExtensionError("blocked peer lane did not resume".to_string()))??;
        Ok(())
    }
}
