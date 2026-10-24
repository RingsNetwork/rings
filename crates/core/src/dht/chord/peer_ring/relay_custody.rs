//! Relay custody of a [`PeerRing`]: its own declaration, what it knows of its neighbours', and
//! the holder its view names for a relay key (see `dht::relay_holder`).
//!
//! Every decision runs on one topology snapshot together with the neighbourhood, which is
//! locked inside the snapshot's observation:
//!
//! ```text
//! Law (Lock order) : topology_transition ≺ relay_neighborhood
//! ```
//!
//! The neighbourhood lock is a leaf: nothing is acquired while it is held. So is the lock of the
//! unacknowledged holds and of the holder hints, which no other lock is held across.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::MutexGuard;

use super::PeerRing;
use crate::consts::UNACKNOWLEDGED_RELAY_HOLDS_MAX;
use crate::dht::entry::EntryOperation;
use crate::dht::relay_holder::relay_holder;
use crate::dht::relay_holder::PreviousHolder;
use crate::dht::relay_holder::RelayCustody;
use crate::dht::relay_holder::RelayHolder;
use crate::dht::relay_holder::RelayNeighborhood;
use crate::dht::relay_holder::RelaySegment;
use crate::dht::topology::TopologyState;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::RelayHoldDigest;

/// A relay hold this node wrote and keeps until a holder acknowledges it.
#[derive(Debug)]
pub(crate) struct KeptRelayHold {
    /// The stamped hold, resent byte for byte.
    hold: EntryOperation,
    /// The node it was last sent to, the only one whose acknowledgement or refusal names it;
    /// `None` while no view names a holder.
    target: Option<Did>,
}

/// What this node asked of, and was told by, the owners of the relay keys it keeps and does not
/// place (`Law (Owner places)`).
///
/// Law (Asked): an answer is taken only for a key with a query outstanding, and taking it ends
/// the query, so each query is answered at most once, by the first answer to arrive, and a peer
/// can neither name a holder unasked nor overwrite a hint between queries. Both maps keep only
/// the keys still unplaced, so they are bounded by the carriers kept here.
#[derive(Debug, Default)]
pub(crate) struct RelayHolderHints {
    /// The keys whose owner was asked and has not answered yet.
    asked: BTreeSet<Did>,
    /// The holder each owner named, by key.
    named: BTreeMap<Did, Did>,
}

impl PeerRing {
    /// Whether this node holds relay mail: it keeps a retained relay store.
    pub fn holds_relay_mail(&self) -> bool {
        self.relay_storage.is_some()
    }

    /// The relay custody this node declares when it connects.
    pub fn relay_custody(&self) -> RelayCustody {
        RelayCustody::of(self.holds_relay_mail())
    }

    /// Lock the neighbourhood, a leaf of `Law (Lock order)`.
    fn lock_relay_neighborhood(&self) -> Result<MutexGuard<'_, RelayNeighborhood>> {
        self.relay_neighborhood
            .lock()
            .map_err(|_| Error::LockPoisoned)
    }

    /// Run `observe` on one topology snapshot and the neighbourhood together.
    fn with_relay_view<T>(
        &self,
        observe: impl FnOnce(&TopologyState, &mut RelayNeighborhood) -> T,
    ) -> Result<T> {
        self.with_topology_state(|state| {
            self.lock_relay_neighborhood()
                .map(|mut neighborhood| observe(state, &mut neighborhood))
        })?
    }

    /// Record the declaration `peer` made in its connection handshake.
    pub(crate) fn observe_relay_custody(&self, peer: Did, custody: RelayCustody) -> Result<()> {
        self.lock_relay_neighborhood()?.observe(peer, custody);
        Ok(())
    }

    /// Forget what is known of `peer`'s relay custody, once it left this node's topology.
    pub(crate) fn forget_relay_custody(&self, peer: Did) -> Result<()> {
        self.lock_relay_neighborhood()?.forget(peer);
        Ok(())
    }

    /// The peers that declared they hold no relay mail.
    pub(crate) fn relay_storage_less_peers(&self) -> Result<Vec<Did>> {
        Ok(self.lock_relay_neighborhood()?.storage_less_peers())
    }

    /// Forget the declarations of `stale`, the peers this node no longer deals with, except
    /// those its topology references, so the record stays bounded by the peers it still knows.
    pub(crate) fn forget_stale_relay_custody(&self, stale: &[Did]) -> Result<()> {
        self.with_relay_view(|state, neighborhood| {
            neighborhood.retain(|peer| !stale.contains(&peer) || state.references(peer));
        })
    }

    /// The segment of the ring this view places relay keys in.
    pub(crate) fn relay_segment(&self) -> Result<RelaySegment> {
        let holds = self.holds_relay_mail();
        self.with_relay_view(|state, neighborhood| neighborhood.segment(state, holds))
    }

    /// The holder this view names for the relay key `key` (`Law (Holder)`).
    pub(crate) fn relay_holder(&self, key: Did) -> Result<RelayHolder> {
        Ok(relay_holder(&self.relay_segment()?, key))
    }

    /// Whether this node holds the relay key `key` under its own view.
    pub(crate) fn holds_relay_key(&self, key: Did) -> Result<bool> {
        Ok(self.relay_holder(key)? == RelayHolder::Holder(self.did))
    }

    /// Take the previous holder `previous` that `from` passed, iff `from` is this node's
    /// predecessor; the result says whether it was taken.
    pub(crate) fn accept_previous_relay_holder(
        &self,
        from: Did,
        previous: PreviousHolder,
    ) -> Result<bool> {
        self.with_relay_view(|state, neighborhood| neighborhood.pass(state, from, previous))
    }

    /// The previous holder this node passes to its successor head, with that head.
    pub(crate) fn previous_relay_holder_to_pass(&self) -> Result<Option<(Did, PreviousHolder)>> {
        let holds = self.holds_relay_mail();
        self.with_relay_view(|state, neighborhood| neighborhood.record_to_pass(state, holds))
    }

    /// Whether `writer` may hold a message for `destination` here (see `HoldAuthority`).
    pub(crate) fn admits_relay_hold(&self, destination: Did, writer: Did) -> Result<bool> {
        self.with_relay_view(|state, neighborhood| {
            neighborhood
                .hold_authority(state, destination)
                .admits(writer, |peer| neighborhood.holds(peer))
        })
    }

    /// Lock the unacknowledged holds, a leaf of `Law (Lock order)`.
    fn lock_unacknowledged_relay_holds(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<RelayHoldDigest, KeptRelayHold>>> {
        self.unacknowledged_relay_holds
            .lock()
            .map_err(|_| Error::LockPoisoned)
    }

    /// Keep `hold`, of digest `digest`, which this node is about to send to `target` (`None`
    /// when its view names no holder), until a holder acknowledges it (`Law (Acknowledged)`).
    ///
    /// Post: a hold kept already has its target updated; a new hold beyond
    /// [`UNACKNOWLEDGED_RELAY_HOLDS_MAX`] is refused with `RelayHoldsUnacknowledgedFull`, and no
    /// earlier hold is forgotten for it.
    pub(crate) fn await_relay_hold(
        &self,
        digest: RelayHoldDigest,
        hold: EntryOperation,
        target: Option<Did>,
    ) -> Result<()> {
        let mut kept = self.lock_unacknowledged_relay_holds()?;
        if let Some(known) = kept.get_mut(&digest) {
            known.target = target;
            return Ok(());
        }
        if kept.len() >= UNACKNOWLEDGED_RELAY_HOLDS_MAX {
            return Err(Error::RelayHoldsUnacknowledgedFull);
        }
        kept.insert(digest, KeptRelayHold { hold, target });
        Ok(())
    }

    /// Stop keeping the hold `digest` iff `from` is the node it was last sent to: the holder that
    /// took it acknowledged it. The result says whether it was kept and is now forgotten.
    pub(crate) fn acknowledge_relay_hold(
        &self,
        digest: RelayHoldDigest,
        from: Did,
    ) -> Result<bool> {
        let mut kept = self.lock_unacknowledged_relay_holds()?;
        let acknowledged = kept
            .get(&digest)
            .is_some_and(|known| known.target == Some(from));
        if acknowledged {
            kept.remove(&digest);
        }
        Ok(acknowledged)
    }

    /// The hold `digest`, iff it is kept and `from`, which refused it, is the node it was last
    /// sent to: a refusal names only a hold this node wrote and sent there.
    pub(crate) fn refused_relay_hold(
        &self,
        digest: RelayHoldDigest,
        from: Did,
    ) -> Result<Option<EntryOperation>> {
        Ok(self
            .lock_unacknowledged_relay_holds()?
            .get(&digest)
            .filter(|known| known.target == Some(from))
            .map(|known| known.hold.clone()))
    }

    /// Stop keeping the hold `digest`: it took effect here, it is refused for good, or its
    /// sender proof expired.
    pub(crate) fn forget_relay_hold(&self, digest: RelayHoldDigest) -> Result<()> {
        self.lock_unacknowledged_relay_holds()?.remove(&digest);
        Ok(())
    }

    /// Every kept hold with its digest, to send again.
    pub(crate) fn unacknowledged_relay_holds(
        &self,
    ) -> Result<Vec<(RelayHoldDigest, EntryOperation)>> {
        Ok(self
            .lock_unacknowledged_relay_holds()?
            .iter()
            .map(|(digest, known)| (*digest, known.hold.clone()))
            .collect())
    }

    /// Lock the holder hints, a leaf of `Law (Lock order)`.
    fn lock_relay_holder_hints(&self) -> Result<MutexGuard<'_, RelayHolderHints>> {
        self.relay_holder_hints
            .lock()
            .map_err(|_| Error::LockPoisoned)
    }

    /// The keys of the live relay carriers kept here whose owner this view does not place: the
    /// carriers to ask their key's owner about (`FindRelayHolder`, `Law (Owner places)`). The
    /// holder hints keep only these keys, so they are bounded by the relay store.
    pub(crate) async fn unplaced_relay_keys(&self, now_ms: u128) -> Result<Vec<Did>> {
        let segment = self.relay_segment()?;
        let unplaced = self
            .live_storage_entries(now_ms)
            .await?
            .into_iter()
            .filter(|(key, _)| key.kind().is_relay_inbox())
            .map(|(key, _)| key.placement())
            .filter(|key| relay_holder(&segment, *key) == RelayHolder::Unknown)
            .collect::<Vec<_>>();
        let mut hints = self.lock_relay_holder_hints()?;
        hints.asked.retain(|key| unplaced.contains(key));
        hints.named.retain(|key, _| unplaced.contains(key));
        Ok(unplaced)
    }

    /// Record that the owner of `key` is asked for its holder (`Law (Asked)`).
    pub(crate) fn ask_relay_holder(&self, key: Did) -> Result<()> {
        self.lock_relay_holder_hints()?.asked.insert(key);
        Ok(())
    }

    /// Take `holder`, the answer to the outstanding query for `key` (`Law (Asked)`): end the
    /// query, and record `holder` iff it is another node that holds relay mail as far as this
    /// node knows. The result says whether the hint changed, so only a new holder costs a pass.
    pub(crate) fn name_relay_holder(&self, key: Did, holder: Option<Did>) -> Result<bool> {
        let holds = |holder: Did| -> Result<bool> {
            Ok(holder != self.did && self.lock_relay_neighborhood()?.holds(holder))
        };
        let holder = match holder {
            Some(holder) if holds(holder)? => Some(holder),
            _ => None,
        };
        let mut hints = self.lock_relay_holder_hints()?;
        if !hints.asked.remove(&key) {
            return Ok(false);
        }
        let changed = match holder {
            Some(holder) => hints.named.insert(key, holder) != Some(holder),
            None => hints.named.remove(&key).is_some(),
        };
        Ok(changed)
    }

    /// The holders the keys' owners named, by key.
    pub(crate) fn relay_holder_hints(&self) -> Result<BTreeMap<Did, Did>> {
        Ok(self.lock_relay_holder_hints()?.named.clone())
    }

    /// Whether this node takes a relay carrier relocated by `sender` (`relocates_from`).
    pub(crate) fn relocates_relay_from(&self, sender: Did) -> Result<bool> {
        Ok(self.lock_relay_neighborhood()?.relocates_from(sender))
    }
}
