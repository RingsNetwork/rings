#![warn(missing_docs)]
//! Registry / router — type-erases pure [`Protocol`]s into uniform handlers and routes
//! decoded [`Envelope`]s to them by namespace, driving the bounded re-injection fixpoint.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

use rings_core::dht::Did;

use super::compute::ComputeFn;
use super::compute::ComputeServices;
use super::envelope::Envelope;
use super::interpreter::Interpreter;
use super::protocol::Ctx;
use super::protocol::Event;
use super::protocol::Inbound;
use super::protocol::Protocol;
use super::protocol::Transition;
use super::MaybeSend;
use crate::error::Error;
use crate::error::Result;

/// Upper bound on re-injection iterations per inbound message, so a misbehaving
/// protocol/effect cycle cannot diverge. The driver computes a bounded fixpoint.
const MAX_FIXPOINT_STEPS: u32 = 1024;

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
    /// protocol registers are visible when its [`Effect::Compute`](super::Effect::Compute)s
    /// run.
    pub fn computes(&self) -> ComputeServices {
        self.compute.clone()
    }

    /// Register an impure [`ComputeFn`] for `namespace` (see
    /// [`Effect::Compute`](super::Effect::Compute)).
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
