//! The data plane's shell: it owns the admission state, applies the pure hop step to it, and
//! performs the outcome (#834 L9, #843 item 3).
//!
//! ```text
//! Cell(from, cell): now ← clock
//!                   now < clock_A − X₀ ?  ⇒ renew(fresh e′, key, A's live links); the epoch
//!                                            cell ← e′, published at the next heartbeat
//!                   outcome ← hop(A, d, relays, T.expects(from), from, now, cell)     (pure)
//!                     Relayed(next, c)         ⇒ link sender ← (next, c)       (next is live)
//!                     Consumed(f, ā, v, υ)     ⇒ ⟦f⟧(from, ā, v, υ) in the algebra
//!                     Returned(t_⋄, c)         ⇒ T.deliver(t_⋄, c)
//!                     Refused | Dropped        ⇒ nothing (the cell was paid for, or not decrypted)
//! Link(Opened(p))   p live ⇒ nothing;  else A.link_opened((p, fresh g)): a full table leaves it
//!                   not live, else the emitter opens p's lane
//! Link(Closed(p))   A.link_closed(p's live link);  the emitter closes p's lane;  fate(p)
//! Link(Dropped(p))  fate(p): a message lost from p's live link may be theirs (#966, #969)
//! Link(Reconcile)   L ← core's admitted peers, read now;  A.reconcile(L);  lanes ← A's live peers
//! fate(p)           T.lose(p);  ⟦−⟧.lose(p)
//! ```
//!
//! Every effect runs under the protocol's transition gate, so cells and link facts reach the
//! admission state in one linear order. Core's facts arrive in its order and are never dropped
//! (`extension::dispatch`); the one `Reconcile`, injected at install, reads core's admitted
//! peers inside its effect, at its place in that order: the linearisation obligation of
//! admission (#844). A renewal keeps the table's own live links, which are linearised by
//! construction, and draws the new epoch independently and uniformly, retrying on the negligible
//! collision with the current one. Lanes change only after the table, and only toward the links
//! the table holds live.
//!
//! Law (Circuit fate, #963, #966, #969). Relays keep no session state, so a link fact reaches
//! only the sessions of this node's own endpoints that ride the link: a client session with a
//! loop out through `p` and back (`T.lose`, `tags`), and an exit session whose loops arrive from
//! `p` or whose replies leave through `p` (`⟦−⟧.lose`, `session::serve`). On `Closed(p)`, and on
//! `Dropped(p)`, where a lost cell may have been one of theirs, each fails closed at this node
//! and sends `abort` toward its other endpoint, which therefore observes the failure too, at the
//! latest at its next loop. A cell lost at a relay is observed by both endpoints as a sequence
//! gap, which fails the session closed the same way (`session`). A session with no reply in
//! flight sees no gap; its liveness rule observes the death of a link that is neither endpoint's
//! own instead: `h` echoes the client's keep-alive, the client fails a session silent for
//! `Δ = 2V` and aborts it toward `h`, and `h` drops a session without a forward loop for `V`
//! (`session`, D2′ Liveness, #975).

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::utils::get_epoch_ms;
use rings_runtime::MaybeSendSync;

use super::admission::OnionAdmissionLink;
use super::admission::OnionAdmissionState;
use super::admission::OnionReplayFilterKey;
use super::codec::OnionLinkFact;
use super::hop::hop;
use super::hop::OnionHopOutcome;
use super::OnionCircuitInput;
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

    /// The link to `peer` retired, or a message from it was dropped here: fail closed every
    /// session of this symbol that rides it (`Law (Circuit fate)` of the module). Required, so
    /// no interpretation can leave such a session waiting; it must not block.
    fn lose(&self, peer: Did);
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
    /// `⟦−⟧.lose(peer)`: every interpretation fails closed its sessions that ride the link to
    /// `peer`.
    fn lose(&self, peer: Did) {
        self.interpretations
            .values()
            .for_each(|interpretation| interpretation.lose(peer));
    }

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
    /// The generation of the next admission the table opens (`OnionAdmissionLink`).
    generations: AtomicU64,
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
            generations: AtomicU64::new(0),
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

    /// `Cell(from, cell)` of the module diagram.
    async fn hop(
        &self,
        scope: &EffectScope,
        from: Did,
        cell: crate::onion::sphinx::cell::OnionCell,
    ) -> Result<()> {
        let now_ms = get_epoch_ms();
        self.renew_if_rolled_back(now_ms)?;
        let outcome = {
            let mut admission = lock(&self.admission)?;
            hop(
                &mut admission,
                &self.key,
                self.relays,
                |tag| self.tags.expects(from, tag),
                from,
                now_ms,
                cell,
            )
        };
        match outcome {
            OnionHopOutcome::Relayed { next, cell } => {
                // A full lane drops the cell: the loop's session sees a gap and fails closed.
                if let Err(error) = self.link_sender.enqueue(
                    scope.lifecycle(),
                    OnionLink::new(next),
                    Bytes::from(cell.into_bytes()),
                ) {
                    tracing::debug!(%next, %error, "drop a relayed onion cell");
                }
                Ok(())
            }
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
                if let Err(dropped) = self.tags.deliver(now_ms, from, &tag, cell) {
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

    /// Apply one link fact (see the module diagram): first to the admission table, then to the
    /// link emitter, whose lanes follow the links the table holds live, then to the sessions
    /// that ride the link (`Law (Circuit fate)`).
    async fn link(&self, scope: &EffectScope, fact: OnionLinkFact) -> Result<()> {
        let now_ms = get_epoch_ms();
        let lifecycle = scope.lifecycle();
        let (refused, lanes) = match fact {
            OnionLinkFact::Opened(peer) => self.open_link(now_ms, peer)?,
            OnionLinkFact::Closed(peer) => {
                let mut admission = lock(&self.admission)?;
                if let Some(link) = admission.live_link(peer) {
                    admission.link_closed(now_ms, link);
                }
                (Vec::new(), OnionLaneChange::Close(peer))
            }
            OnionLinkFact::Dropped(_) => (Vec::new(), OnionLaneChange::Keep),
            OnionLinkFact::Reconcile => self.reconcile(now_ms, lifecycle.admitted_peers())?,
        };
        let applied = match lanes {
            OnionLaneChange::Keep => Ok(()),
            OnionLaneChange::Open(peer) => self.link_sender.open(lifecycle, OnionLink::new(peer)),
            OnionLaneChange::Close(peer) => self.link_sender.close(OnionLink::new(peer)),
            OnionLaneChange::Follow(up) => self.link_sender.reconcile(&lifecycle, &up),
        };
        if let Err(error) = applied {
            tracing::debug!(%error, "onion link lanes did not follow a link fact");
        }
        if let OnionLinkFact::Closed(peer) | OnionLinkFact::Dropped(peer) = fact {
            self.tags.lose(peer);
            self.algebra.lose(peer);
        }
        #[cfg(all(test, rings_native))]
        if let Ok(mut waiters) = lock(&self.link_waiters) {
            waiters.drain(..).for_each(|waiter| {
                let _ = waiter.send(());
            });
        }
        for link in refused {
            tracing::debug!(
                did = %link.peer(),
                "onion link table is full; the link carries no onion cell"
            );
        }
        Ok(())
    }

    /// `Opened(peer)`: a fresh generation of `peer`'s link, unless one is live already.
    fn open_link(
        &self,
        now_ms: u128,
        peer: Did,
    ) -> Result<(Vec<OnionAdmissionLink>, OnionLaneChange)> {
        let mut admission = lock(&self.admission)?;
        if admission.live_link(peer).is_some() {
            return Ok((Vec::new(), OnionLaneChange::Keep));
        }
        let link = OnionAdmissionLink::new(peer, self.next_generation());
        Ok(match admission.link_opened(now_ms, link) {
            Ok(()) => (Vec::new(), OnionLaneChange::Open(peer)),
            Err(_) => (vec![link], OnionLaneChange::Keep),
        })
    }

    /// `Reconcile`: make the live links those of `admitted`, core's admitted peers read now,
    /// keeping the generation of every peer already live.
    fn reconcile(
        &self,
        now_ms: u128,
        admitted: Vec<Did>,
    ) -> Result<(Vec<OnionAdmissionLink>, OnionLaneChange)> {
        let mut admission = lock(&self.admission)?;
        let snapshot = admitted
            .into_iter()
            .map(|peer| {
                admission
                    .live_link(peer)
                    .unwrap_or_else(|| OnionAdmissionLink::new(peer, self.next_generation()))
            })
            .collect::<Vec<_>>();
        let refused = admission.reconcile(now_ms, snapshot).links().to_vec();
        Ok((refused, OnionLaneChange::Follow(live_peers(&admission))))
    }

    /// A generation no admission of this table has had.
    fn next_generation(&self) -> u64 {
        self.generations.fetch_add(1, Ordering::Relaxed)
    }

    /// Renew `A` into a fresh epoch if the clock has rolled back beyond `X₀`, and set the
    /// node's epoch cell to it, which the registrations publish at their next heartbeat. The
    /// renewed table keeps its own live links, which are linearised with the facts it has
    /// applied, so it refuses none.
    fn renew_if_rolled_back(&self, now_ms: u128) -> Result<()> {
        let mut admission = lock(&self.admission)?;
        if !admission.is_rolled_back_at(now_ms) {
            return Ok(());
        }
        loop {
            let epoch = OnionProcessEpoch::random();
            let key = OnionReplayFilterKey::new(rand::random());
            if admission.renew(epoch, key).is_ok() {
                self.epoch.renew(epoch);
                tracing::warn!("onion admission renewed its epoch after a clock rollback");
                return Ok(());
            }
            // The fresh draw met the current epoch (probability 2⁻¹²⁸): draw again.
        }
    }
}

impl Drop for OnionCircuitShell {
    /// Law (lifetime): the data plane lives as long as its registration, which owns the shell,
    /// not as long as any handle to it: a relay whose runtime handle was dropped still relays.
    /// When the registry drops the shell, the link sender shuts down, which closes every lane,
    /// stops its emitter, and refuses every later open, so no onion task outlives the protocol.
    fn drop(&mut self) {
        if let Err(error) = self.link_sender.shutdown() {
            tracing::debug!(%error, "onion data plane could not close its link lanes");
        }
    }
}

/// What one link fact does to the emitter's lanes.
enum OnionLaneChange {
    /// Nothing.
    Keep,
    /// The peer's link is up.
    Open(Did),
    /// No generation of the peer's link is live.
    Close(Did),
    /// Exactly these peers' links are up.
    Follow(Vec<Did>),
}

/// The peers with a live link in `admission`, each once.
fn live_peers(admission: &OnionAdmissionState) -> Vec<Did> {
    let mut peers = admission
        .live_links()
        .into_iter()
        .map(OnionAdmissionLink::peer)
        .collect::<Vec<_>>();
    peers.dedup();
    peers
}

#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
impl Interpret for OnionCircuitShell {
    type Effect = OnionCircuitInput;

    async fn run(&self, scope: &EffectScope, effect: OnionCircuitInput) -> Result<Vec<Bytes>> {
        match effect {
            OnionCircuitInput::Cell { from, cell } => self.hop(scope, from, cell).await,
            OnionCircuitInput::Link(fact) => self.link(scope, fact).await,
        }
        .map(|()| Vec::new())
        .or_else(|error: Error| {
            tracing::debug!(%error, "onion circuit effect failed");
            Ok(Vec::new())
        })
    }
}
