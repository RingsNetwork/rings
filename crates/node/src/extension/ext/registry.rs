#![warn(missing_docs)]
//! Router + capability core.
//!
//! [`Extensions`] registers `(Protocol, Interpret)` pairs by namespace. [`Core`] is the
//! small capability handle the runtime hands every interpreter — overlay `send`, `did`, and
//! `inject` — and is also the entry point that routes an inbound [`Envelope`] to its
//! protocol and drives the bounded re-injection fixpoint. The registry stays uniform
//! (everything erased to [`Handler`]) while each extension's shell is its own.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

use bytes::Bytes;
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
#[cfg(not(feature = "browser"))]
pub type DynHandler = dyn Handler + Send + Sync;
/// Type-erased handler stored in the registry.
#[cfg(feature = "browser")]
pub type DynHandler = dyn Handler;

type HandlerMap = RwLock<HashMap<String, Arc<DynHandler>>>;

/// Erased, runtime-facing handler. Implemented once, generically, by `Runner`.
#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
pub trait Handler {
    /// Routed namespace.
    fn namespace(&self) -> &str;
    /// Decode → step (pure, committed) → run the protocol's effects, returning re-injected
    /// messages. `handle : (from, payload) → IO [Inbound]`.
    async fn handle(&self, core: &Core, from: Did, payload: Bytes) -> Result<Vec<Inbound>>;
}

/// The small capability surface handed to every interpreter. Cloneable and `'static` so a
/// long-running engine task (e.g. a relay listener) can keep a copy and feed events back via
/// [`inject`](Core::inject). Deliberately tiny: a P2P node's only universal capability is to
/// put a message on the overlay.
#[derive(Clone)]
pub struct Core {
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
    pub async fn dispatch(&self, from: Did, envelope: Envelope) -> Result<()> {
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

/// Adapter binding a pure [`Protocol`] to its [`Interpret`] shell and owned state; erased to
/// [`Handler`]. Protocol authors never write this.
struct Runner<P: Protocol, I> {
    protocol: P,
    interpret: I,
    state: Mutex<P::State>,
}

#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
impl<P, I> Handler for Runner<P, I>
where
    P: Protocol + MaybeSend + 'static,
    P::State: MaybeSend + 'static,
    P::Effect: MaybeSend,
    I: Interpret<Effect = P::Effect> + MaybeSend + 'static,
{
    fn namespace(&self) -> &str {
        self.protocol.namespace()
    }

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

        // Impure region (lock released): run the protocol's own effects via its interpreter.
        let mut reinjected = Vec::new();
        for effect in effects {
            reinjected.extend(self.interpret.run(core, effect).await?);
        }
        Ok(reinjected)
    }
}

/// Registry of `(Protocol, Interpret)` pairs by namespace, plus the capability [`Core`].
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

    /// The capability handle, for code that needs to dispatch / inject / send directly.
    pub fn core(&self) -> Core {
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

    /// Route a decoded envelope (inbound entry point). See [`Core::dispatch`].
    pub async fn dispatch(&self, from: Did, envelope: Envelope) -> Result<()> {
        self.core.dispatch(from, envelope).await
    }
}
