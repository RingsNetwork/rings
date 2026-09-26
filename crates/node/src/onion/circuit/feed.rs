//! The link feed: core's link facts, in order, into the data plane (#843 item 3; the
//! event-pairing and linearisation obligations of admission, #844).
//!
//! ```text
//! Backend::on_event ──(sync, before any await)──▶ observe(fact) ──try_send──▶ FIFO (bounded)
//! tick every V/2 ──────────────────────────────────────────────────────────▶ FIFO: Reconcile
//! callback replaced ─────────────────────────────────────────────────────────▶ FIFO: Reconcile
//! FIFO ──drain, in order──▶ inject into the data plane's own namespace ──▶ Link(fact)
//! ```
//!
//! Laws:
//!
//! - **Order.** Facts reach the data plane in the order core delivered them: one FIFO, one
//!   drain, and the protocol's transition gate after it.
//! - **Repair.** A fact lost to a full FIFO (or to a replaced callback) is repaired by the next
//!   `Reconcile`, at most `V/2` later, which reads core's registry when it is applied, so the
//!   link table agrees with core up to refusals within one tick.

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

/// Link facts buffered between core's callback and the drain: several reconnect bursts of a full
/// connection registry.
const ONION_LINK_FEED_CAPACITY: usize = 1_024;

/// The reconcile period, `V/2`: within the `V` the event-pairing obligation allows.
const ONION_RECONCILE_PERIOD_MS: u128 = ONION_FORWARD_MAX_VALIDITY_MS / 2;

/// The observing end of the feed, registered with the node's link observers.
pub(crate) struct OnionLinkFeed {
    /// The FIFO's sending end.
    facts: mpsc::Sender<OnionLinkFact>,
}

impl OnionLinkFeed {
    /// Start the feed for the data plane behind `scope`: its drain and its tick run as long as
    /// the node's runtime, and the returned observer fills the FIFO.
    pub(crate) fn start(scope: Scope) -> Result<Self> {
        let spawner = Spawner::current()?;
        let (facts, mut drained) = mpsc::channel::<OnionLinkFact>(ONION_LINK_FEED_CAPACITY);
        spawner.spawn(async move {
            while let Some(fact) = drained.next().await {
                let injected = match fact.encode() {
                    Ok(payload) => scope.inject(payload).await,
                    Err(error) => Err(error),
                };
                if let Err(error) = injected {
                    tracing::debug!(%error, "failed to inject an onion link fact");
                }
            }
        });
        let mut ticks = facts.clone();
        spawner.spawn(async move {
            loop {
                if ticks.try_send(OnionLinkFact::Reconcile).is_err() && ticks.is_closed() {
                    return;
                }
                let period = std::time::Duration::from_millis(
                    u64::try_from(ONION_RECONCILE_PERIOD_MS).unwrap_or(u64::MAX),
                );
                if sleep(period).await.is_err() {
                    return;
                }
            }
        });
        Ok(Self { facts })
    }
}

impl LinkObserver for OnionLinkFeed {
    /// Queue the fact; a full FIFO drops it, to be repaired by the next reconciliation.
    fn observe(&self, fact: LinkFact) {
        let fact = match fact {
            LinkFact::Transition(link, PeerTransition::Admitted) => OnionLinkFact::opened(link),
            LinkFact::Transition(link, PeerTransition::Retired) => OnionLinkFact::closed(link),
            LinkFact::CallbackReplaced => OnionLinkFact::Reconcile,
        };
        if self.facts.clone().try_send(fact).is_err() {
            tracing::debug!(
                ?fact,
                "onion link feed is full; the next reconcile repairs it"
            );
        }
    }
}
