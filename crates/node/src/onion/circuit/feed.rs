//! The link feed: core's link facts, in order, into the data plane (#843 item 3; the
//! event-pairing and linearisation obligations of admission, #844).
//!
//! ```text
//! Backend::on_event ──(sync, before any await)──▶ observe(fact) ─┐
//! callback replaced ──────▶ observe(CallbackReplaced) ──snapshot─┤  under the feed's lock:
//! tick every V/2 ─────────▶ push_snapshot() ────────────snapshot─┤  push one fact on the FIFO
//!                                                                ▼
//! FIFO (bounded) ──drain, in order──▶ inject into the data plane's own namespace ──▶ Link(fact)
//! ```
//!
//! Laws:
//!
//! - **Order.** Facts reach the data plane in the order core delivered them: one FIFO, one
//!   drain, and the protocol's transition gate after it.
//! - **Linearisation.** A `Reconcile` carries core's registry snapshot, read under the lock that
//!   also orders every pushed fact. Its place in the FIFO is therefore the instant it was read:
//!   every fact before it is older, and every fact after it agrees with it or describes a later
//!   change (core's `admitted_links` law). The admission table and the lanes apply it as
//!   authoritative at that place.
//! - **Bound.** The FIFO holds at most `ONION_LINK_FEED_CAPACITY` facts: one stored sender is
//!   used for every push, so a full FIFO refuses. A refused fact is repaired by the next
//!   `Reconcile`, at most `V/2` later.
//! - **Lifetime.** The feed lives as long as its owner (the node's onion runtime) holds it: the
//!   registry and the tick hold it weakly, and the drain ends when the feed, the only sender, is
//!   dropped.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Duration;

use futures::channel::mpsc;
use futures::StreamExt;
use rings_core::swarm::callback::PeerTransition;
use rings_runtime::sleep;
use rings_runtime::Spawner;

use super::codec::OnionLinkFact;
use super::ONION_FORWARD_MAX_VALIDITY_MS;
use crate::error::Result;
use crate::extension::ext::LinkFact;
use crate::extension::ext::LinkObserver;
use crate::extension::ext::Scope;
use crate::sync_lock::lock;

/// Link facts buffered between core's callback and the drain: several reconnect bursts of a full
/// connection registry.
const ONION_LINK_FEED_CAPACITY: usize = 1_024;

/// The reconcile period, `V/2`: within the `V` the event-pairing obligation allows.
const ONION_RECONCILE_PERIOD: Duration = Duration::from_millis(75_000);

const _: () = assert!(ONION_RECONCILE_PERIOD.as_millis() == ONION_FORWARD_MAX_VALIDITY_MS / 2);

/// The observing end of the feed, registered with the node's link observers.
pub(crate) struct OnionLinkFeed {
    /// The FIFO's one sending end, whose lock orders every push and every snapshot read.
    facts: Mutex<mpsc::Sender<OnionLinkFact>>,
    /// The data plane's scope, whose core the snapshots are read from.
    scope: Scope,
}

impl OnionLinkFeed {
    /// Start the feed for the data plane behind `scope`: its drain runs until the returned feed
    /// is dropped, and its tick reconciles every `V/2` while the feed lives.
    ///
    /// # Errors
    ///
    /// No runtime to spawn the drain and the tick on.
    pub(crate) fn start(scope: Scope) -> Result<Arc<Self>> {
        let spawner = Spawner::current()?;
        let (facts, mut drained) = mpsc::channel::<OnionLinkFact>(ONION_LINK_FEED_CAPACITY);
        let drain_scope = scope.clone();
        spawner.spawn(async move {
            while let Some(fact) = drained.next().await {
                let injected = match fact.encode() {
                    Ok(payload) => drain_scope.inject(payload).await,
                    Err(error) => Err(error),
                };
                if let Err(error) = injected {
                    tracing::debug!(%error, "failed to inject an onion link fact");
                }
            }
        });
        let feed = Arc::new(Self {
            facts: Mutex::new(facts),
            scope,
        });
        let ticking = Arc::downgrade(&feed);
        spawner.spawn(async move { tick(ticking).await });
        Ok(feed)
    }

    /// Push `fact` under the feed's lock.
    fn push_fact(&self, fact: OnionLinkFact) {
        self.push(|| Some(fact));
    }

    /// Push core's registry snapshot as a `Reconcile`, read under the feed's lock.
    fn push_snapshot(&self) {
        self.push(|| match self.scope.admitted_links() {
            Ok(snapshot) => Some(OnionLinkFact::Reconcile(snapshot)),
            Err(error) => {
                tracing::debug!(%error, "onion link feed could not read core's links");
                None
            }
        });
    }

    /// Build a fact with `fact` while holding the feed's lock and push it; a full FIFO refuses
    /// it, to be repaired by the next reconciliation.
    fn push(&self, fact: impl FnOnce() -> Option<OnionLinkFact>) {
        let Ok(mut facts) = lock(&self.facts) else {
            return;
        };
        if let Some(fact) = fact() {
            if facts.try_send(fact).is_err() {
                tracing::debug!("onion link feed is full; the next reconcile repairs it");
            }
        }
    }
}

/// Reconcile every `V/2` while the feed lives.
async fn tick(feed: Weak<OnionLinkFeed>) {
    loop {
        let Some(live) = feed.upgrade() else {
            return;
        };
        live.push_snapshot();
        drop(live);
        if sleep(ONION_RECONCILE_PERIOD).await.is_err() {
            return;
        }
    }
}

impl LinkObserver for OnionLinkFeed {
    /// Push the fact; a replaced callback pushes core's snapshot instead.
    fn observe(&self, fact: LinkFact) {
        match fact {
            LinkFact::Transition(link, PeerTransition::Admitted) => {
                self.push_fact(OnionLinkFact::Opened(link));
            }
            LinkFact::Transition(link, PeerTransition::Retired) => {
                self.push_fact(OnionLinkFact::Closed(link));
            }
            LinkFact::CallbackReplaced => self.push_snapshot(),
        }
    }
}
