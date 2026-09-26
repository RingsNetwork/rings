//! The data plane's shell: it owns the admission state, applies the pure hop step to it, and
//! performs the outcome (#834 L9, #843 item 3).
//!
//! ```text
//! Hop(from, cell):  now ← clock
//!                   now < clock_A − X₀ ?  ⇒ renew(fresh e′, key, admitted links), publish e′
//!                   outcome ← hop(A, d, relays, T.contains, from, now, cell)          (pure)
//!                     Relayed(next, c)         ⇒ link sender ← (next, c)
//!                     Consumed(f, ā, v, υ)     ⇒ ⟦f⟧(from, ā, v, υ) in the algebra
//!                     Returned(t_⋄, c)         ⇒ T.deliver(t_⋄, c)
//!                     Refused | Dropped        ⇒ nothing (the cell was paid for, or not decrypted)
//! Link(Opened(l))   A.link_opened(l); a full table ⇒ close l; else the emitter opens l's lane
//! Link(Closed(l))   A.link_closed(l); the emitter closes l's lane
//! Link(Reconcile)   refused ← A.reconcile(core's admitted links, read here); close refused;
//!                   the emitter's lanes ← those links; T.purge(now)
//! ```
//!
//! Every effect runs under the protocol's transition gate, so cells and link facts reach the
//! admission state in one linear order, and the reconcile snapshot is read and applied at that
//! order's drain point: the linearisation obligation of admission (#844). A renewal draws the new
//! epoch independently and uniformly, retrying on the negligible collision with the current one.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::swarm::callback::PeerLink;
use rings_core::utils::get_epoch_ms;
use rings_runtime::MaybeSendSync;

use super::admission::OnionAdmissionLink;
use super::admission::OnionAdmissionState;
use super::admission::OnionReplayFilterKey;
use super::codec::OnionLinkFact;
use super::hop::hop;
use super::hop::OnionHopOutcome;
use super::OnionCircuitEffect;
use super::OnionClientTags;
use super::OnionLink;
use super::OnionLinkSender;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::EffectScope;
use crate::extension::ext::Interpret;
use crate::extension::ext::Scope;
use crate::onion::sphinx::carry::OnionCarryValue;
use crate::onion::sphinx::cell::OnionSurb;
use crate::onion::sphinx::layer::OnionArguments;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionProcessEpochCell;
use crate::onion::OnionServiceName;
use crate::sync_lock::lock;

/// The input of one symbol's interpretation: what `Hop_i` consumed at this hop.
pub(crate) struct OnionApplicationInput {
    /// The authenticated previous hop.
    pub(crate) from: Did,
    /// `ā`.
    pub(crate) arguments: OnionArguments,
    /// `v`.
    pub(crate) value: OnionCarryValue,
    /// `υ`, the reply block of the output.
    pub(crate) surb: Box<OnionSurb>,
    /// When the cell arrived.
    pub(crate) received_at_ms: u128,
}

/// Interpretation `⟦f⟧` of one world-facing symbol `f` at this hop.
///
/// `⟦f⟧(ā) : In_f → M Out_f` is a Kleisli arrow of the hop effect monad `M` (#834 D2): here the
/// input is the consumed value with its reply block, and the effects of `M` (sockets, fetch,
/// reply cells) run in the interpretation's own tasks. `evaluate` runs under the data plane's
/// transition gate, so it hands its input on and returns at once.
#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
pub(crate) trait OnionInterpretation: MaybeSendSync {
    /// Evaluate one application of the symbol.
    async fn evaluate(&self, scope: &Scope, input: OnionApplicationInput) -> Result<()>;
}

/// The partial Σ-algebra of one node: `⟦−⟧ : Σ_n ⇀ Kl(M)`, `Σ_n ⊆ Σ_W`.
///
/// A node registers symbols, never applications (#834 D2): each entry maps one world-facing
/// symbol it serves to its interpretation, so the registered keys are exactly `Σ_n`. `relay` is
/// not a service name and is interpreted by the hop step itself.
///
/// ```text
/// Consumed(f, …) ──table(f)──▶ Some ⟦f⟧ ──▶ ⟦f⟧(scope, input)
///                          └──▶ None     ──▶ dropped (f ∉ Σ_n)
/// ```
///
/// Law: registering a symbol again replaces its interpretation.
#[derive(Default)]
pub(crate) struct OnionAlgebra {
    interpretations:
        BTreeMap<OnionServiceName, Box<rings_runtime::maybe_send_sync!(dyn OnionInterpretation)>>,
}

impl OnionAlgebra {
    /// Register `interpretation` as `⟦symbol⟧`.
    pub(crate) fn register(
        mut self,
        symbol: OnionServiceName,
        interpretation: impl OnionInterpretation + 'static,
    ) -> Self {
        self.interpretations
            .insert(symbol, Box::new(interpretation));
        self
    }

    /// Return `Σ_n`, the symbols this algebra interprets, in name order.
    #[cfg(all(test, rings_native))]
    pub(crate) fn symbols(&self) -> impl Iterator<Item = &OnionServiceName> {
        self.interpretations.keys()
    }

    /// Evaluate one application of `symbol` through its interpretation.
    async fn evaluate(
        &self,
        scope: &Scope,
        symbol: &OnionServiceName,
        input: OnionApplicationInput,
    ) -> Result<()> {
        match self.interpretations.get(symbol) {
            Some(interpretation) => interpretation.evaluate(scope, input).await,
            None => {
                tracing::debug!(
                    symbol = symbol.as_str(),
                    "drop an onion application of an unregistered symbol"
                );
                Ok(())
            }
        }
    }
}

/// The interpreter of the data plane's effects; see the module documentation.
pub(crate) struct OnionCircuitShell {
    /// `d_i`, the key every header is peeled under.
    key: DelegateeKey,
    /// Whether this node registers `relay`.
    relays: bool,
    /// The admission state `A`: ledgers, the link table and the replay store.
    admission: Arc<Mutex<OnionAdmissionState>>,
    /// `e_n`, published by the registrations and renewed with `A`.
    epoch: OnionProcessEpochCell,
    /// The client's tag table `T`.
    tags: OnionClientTags,
    /// The node's constant-rate link emitter.
    link_sender: OnionLinkSender,
    /// `⟦−⟧`.
    algebra: OnionAlgebra,
    /// The waiters of [`OnionLinkWitness::live`], woken after every link fact.
    #[cfg(all(test, rings_native))]
    link_waiters: OnionLinkWaiters,
}

/// The waiters of a link-table change.
#[cfg(all(test, rings_native))]
type OnionLinkWaiters = Arc<Mutex<Vec<futures::channel::oneshot::Sender<()>>>>;

/// A test's view of a shell's link table, for waiting on the event that a link is live instead
/// of on a duration.
#[cfg(all(test, rings_native))]
#[derive(Clone)]
pub(crate) struct OnionLinkWitness {
    /// The shell's admission state.
    admission: Arc<Mutex<OnionAdmissionState>>,
    /// The shell's waiters.
    waiters: OnionLinkWaiters,
}

#[cfg(all(test, rings_native))]
impl OnionLinkWitness {
    /// Wait until the link table holds a live link of `did`.
    ///
    /// No lost wakeup: the check and the waiter's registration happen under the admission lock,
    /// and a link fact wakes the waiters only after it released that lock.
    pub(crate) async fn live(&self, did: Did) {
        loop {
            let woken = {
                let admission = lock(&self.admission).expect("admission lock");
                if admission.live_link(did).is_some() {
                    return;
                }
                let (waiter, woken) = futures::channel::oneshot::channel();
                lock(&self.waiters).expect("waiter lock").push(waiter);
                woken
            };
            let _ = woken.await;
        }
    }
}

impl OnionCircuitShell {
    /// A shell over the node's key and epoch, with a link table of `2·𝓡` for the connection
    /// registry capacity `registry_capacity`, sharing `tags` and `link_sender` with the node's
    /// sessions.
    pub(crate) fn new(
        key: DelegateeKey,
        relays: bool,
        epoch: OnionProcessEpochCell,
        registry_capacity: NonZeroUsize,
        tags: OnionClientTags,
        link_sender: OnionLinkSender,
        algebra: OnionAlgebra,
    ) -> Self {
        let admission = OnionAdmissionState::new(
            epoch.get(),
            OnionReplayFilterKey::new(rand::random()),
            registry_capacity,
        );
        Self {
            key,
            relays,
            admission: Arc::new(Mutex::new(admission)),
            epoch,
            tags,
            link_sender,
            algebra,
            #[cfg(all(test, rings_native))]
            link_waiters: OnionLinkWaiters::default(),
        }
    }

    /// The witness of this shell's link table.
    #[cfg(all(test, rings_native))]
    pub(crate) fn link_witness(&self) -> OnionLinkWitness {
        OnionLinkWitness {
            admission: Arc::clone(&self.admission),
            waiters: Arc::clone(&self.link_waiters),
        }
    }

    /// `Hop(from, cell)` of the module diagram.
    async fn hop(
        &self,
        scope: &EffectScope,
        from: Did,
        cell: crate::onion::sphinx::cell::OnionCell,
    ) -> Result<()> {
        let now_ms = get_epoch_ms();
        let refused = self.renew_if_rolled_back(scope, now_ms)?;
        self.close(scope, refused).await;
        let outcome = {
            let mut admission = lock(&self.admission)?;
            hop(
                &mut admission,
                &self.key,
                self.relays,
                |tag| self.tags.contains(tag),
                from,
                now_ms,
                cell,
            )
        };
        match outcome {
            OnionHopOutcome::Relayed { next, cell } => self.link_sender.enqueue(
                scope.lifecycle(),
                OnionLink::new(next),
                Bytes::from(cell.into_bytes()),
            ),
            OnionHopOutcome::Consumed {
                symbol,
                arguments,
                value,
                surb,
            } => {
                let input = OnionApplicationInput {
                    from,
                    arguments,
                    value,
                    surb,
                    received_at_ms: now_ms,
                };
                self.algebra
                    .evaluate(&scope.lifecycle(), &symbol, input)
                    .await
            }
            OnionHopOutcome::Returned { tag, cell } => {
                if let Err(dropped) = self.tags.deliver(now_ms, &tag, cell) {
                    tracing::debug!(%from, %dropped, "drop a returning onion cell");
                }
                Ok(())
            }
            OnionHopOutcome::Refused(rejection) => {
                tracing::debug!(%from, ?rejection, "refuse an onion cell uncharged");
                Ok(())
            }
            OnionHopOutcome::Dropped(drop) => {
                tracing::debug!(%from, %drop, "drop a charged onion cell");
                Ok(())
            }
        }
    }

    /// Apply one link fact (see the module diagram): to the admission table, and to the link
    /// emitter, whose lanes follow the links that are up.
    async fn link(&self, scope: &EffectScope, fact: OnionLinkFact) -> Result<()> {
        let now_ms = get_epoch_ms();
        let lifecycle = scope.lifecycle();
        let refused = match fact {
            OnionLinkFact::Opened { did, generation } => {
                let link = OnionAdmissionLink { did, generation };
                match lock(&self.admission)?.link_opened(now_ms, link) {
                    Ok(()) => {
                        self.link_sender.open(lifecycle, OnionLink::new(did))?;
                        Vec::new()
                    }
                    Err(_) => vec![link],
                }
            }
            OnionLinkFact::Closed { did, generation } => {
                lock(&self.admission)?.link_closed(now_ms, OnionAdmissionLink { did, generation });
                self.link_sender.close(OnionLink::new(did))?;
                Vec::new()
            }
            OnionLinkFact::Reconcile => {
                self.tags.purge(now_ms);
                let snapshot = lifecycle.admitted_links()?;
                let up = snapshot
                    .iter()
                    .copied()
                    .map(PeerLink::peer)
                    .collect::<Vec<_>>();
                self.link_sender.reconcile(&lifecycle, &up)?;
                lock(&self.admission)?
                    .reconcile(
                        now_ms,
                        snapshot.into_iter().map(|link| OnionAdmissionLink {
                            did: link.peer(),
                            generation: link.generation(),
                        }),
                    )
                    .links()
                    .to_vec()
            }
        };
        #[cfg(all(test, rings_native))]
        if let Ok(mut waiters) = lock(&self.link_waiters) {
            waiters.drain(..).for_each(|waiter| {
                let _ = waiter.send(());
            });
        }
        self.close(scope, refused).await;
        Ok(())
    }

    /// Renew `A` into a fresh epoch if the clock has rolled back beyond `X₀`, publishing the
    /// epoch; return the links the renewed table refuses.
    fn renew_if_rolled_back(
        &self,
        scope: &EffectScope,
        now_ms: u128,
    ) -> Result<Vec<OnionAdmissionLink>> {
        let mut admission = lock(&self.admission)?;
        if !admission.is_rolled_back_at(now_ms) {
            return Ok(Vec::new());
        }
        let snapshot = scope
            .lifecycle()
            .admitted_links()?
            .into_iter()
            .map(|link| OnionAdmissionLink {
                did: link.peer(),
                generation: link.generation(),
            })
            .collect::<Vec<_>>();
        loop {
            let epoch = OnionProcessEpoch::random();
            let key = OnionReplayFilterKey::new(rand::random());
            match admission.renew(epoch, key, snapshot.iter().copied()) {
                Ok(refused) => {
                    self.epoch.renew(epoch);
                    tracing::warn!("onion admission renewed its epoch after a clock rollback");
                    return Ok(refused.links().to_vec());
                }
                // The fresh draw met the current epoch (probability 2⁻¹²⁸): draw again.
                Err(_) => continue,
            }
        }
    }

    /// Close every link in `links`, generation-exactly: the table refused it (fail closed).
    async fn close(&self, scope: &EffectScope, links: Vec<OnionAdmissionLink>) {
        let lifecycle = scope.lifecycle();
        for link in links {
            let peer = PeerLink::new(link.did, link.generation);
            if let Err(error) = lifecycle.disconnect_link(peer).await {
                tracing::debug!(did = %link.did, %error, "failed to close a refused onion link");
            }
        }
    }
}

#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
impl Interpret for OnionCircuitShell {
    type Effect = OnionCircuitEffect;

    async fn run(&self, scope: &EffectScope, effect: OnionCircuitEffect) -> Result<Vec<Bytes>> {
        match effect {
            OnionCircuitEffect::Hop { from, cell } => self.hop(scope, from, cell).await,
            OnionCircuitEffect::Link(fact) => self.link(scope, fact).await,
        }
        .map(|()| Vec::new())
        .or_else(|error: Error| {
            tracing::debug!(%error, "onion circuit effect failed");
            Ok(Vec::new())
        })
    }
}
