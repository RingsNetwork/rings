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
use crate::dht::relay_holder::in_arc;
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
    /// The keys whose owner was asked and has not answered yet, mapping each unplaced
    /// `carrier_key` to `(query_key, request_id)`.
    asked: BTreeMap<Did, (Did, uuid::Uuid)>,
    /// The next stepped `query_key` for an unplaced `carrier_key` after an owner answered
    /// with a storage-less holder or `horizon` in `[query_key, local)`.
    stepped: BTreeMap<Did, Did>,
    /// The holder each owner named, by `carrier_key`.
    named: BTreeMap<Did, Did>,
}

impl RelayHolderHints {
    /// Prune all maps to keys still in `unplaced`.
    fn retain_unplaced(&mut self, unplaced: &[Did]) {
        self.asked.retain(|key, _| unplaced.contains(key));
        self.stepped.retain(|key, _| unplaced.contains(key));
        self.named.retain(|key, _| unplaced.contains(key));
    }

    /// Record `(query_key, request_id)` as the live correlation query for `carrier_key`
    /// (`Law (Asked)`), keeping an existing query while one for `carrier_key` is already
    /// outstanding so a resend does not invalidate an in-flight multi-hop answer.
    fn ask_query(&mut self, carrier_key: Did, request_id: uuid::Uuid) -> (Did, uuid::Uuid) {
        let query_key = self
            .stepped
            .get(&carrier_key)
            .copied()
            .unwrap_or(carrier_key);
        *self
            .asked
            .entry(carrier_key)
            .or_insert((query_key, request_id))
    }

    /// Consume `(key, request_id)` once (`Law (Asked)`) and update the named holder or
    /// stepped query key for the corresponding `carrier_key`, returning whether the hint
    /// or stepped query changed.
    fn take_answer(
        &mut self,
        key: Did,
        request_id: uuid::Uuid,
        holder: Option<Did>,
        next_key: Option<Did>,
    ) -> bool {
        let matched = self
            .asked
            .iter()
            .find_map(|(&carrier_key, &(query_key, token))| {
                (token == request_id && (query_key == key || carrier_key == key))
                    .then_some(carrier_key)
            });
        let Some(carrier_key) = matched else {
            return false;
        };
        self.asked.remove(&carrier_key);
        match holder {
            Some(holder) => {
                self.stepped.remove(&carrier_key);
                self.named.insert(carrier_key, holder) != Some(holder)
            }
            None => {
                let named_removed = self.named.remove(&carrier_key).is_some();
                let stepped_changed = match next_key {
                    Some(next_key) => self.stepped.insert(carrier_key, next_key) != Some(next_key),
                    None => {
                        self.stepped.remove(&carrier_key);
                        false
                    }
                };
                named_removed || stepped_changed
            }
        }
    }
}

impl PeerRing {
    /// Whether this node holds relay mail: it keeps a retained relay store and has been online
    /// for the age gate (`Law (Age gate)`, #777), so a node that leaves within its first minutes
    /// never pays a member's copies.
    pub fn holds_relay_mail(&self) -> bool {
        self.relay_storage.is_some() && self.now_ms() >= self.relay_replication.age_gate_ms
    }

    /// The peers of `connected` this node has not declared to yet that it holds relay mail;
    /// empty while its age gate is closed (`Law (Age gate)`). A peer is declared to once: each
    /// is recorded when its declaration is queued ([`Self::record_relay_custody_declared`]), and
    /// a peer it lost is forgotten with it, so the set is bounded by the peers it knows.
    pub(crate) fn undeclared_relay_custody_peers(&self, connected: Vec<Did>) -> Result<Vec<Did>> {
        if !self.holds_relay_mail() {
            return Ok(Vec::new());
        }
        let declared = self.lock_relay_custody_declared()?;
        Ok(connected
            .into_iter()
            .filter(|peer| !declared.contains(peer))
            .collect())
    }

    /// Record that this node's declaration to `peer` was queued.
    pub(crate) fn record_relay_custody_declared(&self, peer: Did) -> Result<()> {
        self.lock_relay_custody_declared()?.insert(peer);
        Ok(())
    }

    /// Lock the declared-to set, a leaf of `Law (Lock order)`.
    fn lock_relay_custody_declared(&self) -> Result<MutexGuard<'_, BTreeSet<Did>>> {
        self.relay_custody_declared
            .lock()
            .map_err(|_| Error::LockPoisoned)
    }

    /// The members `r` a carrier is replicated to (#777).
    pub(crate) fn relay_replicas(&self) -> usize {
        self.relay_replication.replicas
    }

    /// The relay custody this node declares when it connects.
    pub fn relay_custody(&self) -> RelayCustody {
        RelayCustody::of(self.holds_relay_mail())
    }

    /// Lock the neighbourhood, a leaf of `Law (Lock order)`.
    pub(super) fn lock_relay_neighborhood(&self) -> Result<MutexGuard<'_, RelayNeighborhood>> {
        self.relay_neighborhood
            .lock()
            .map_err(|_| Error::LockPoisoned)
    }

    /// Run `observe` on one topology snapshot and the neighbourhood together.
    pub(super) fn with_relay_view<T>(
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
        if self.lock_relay_neighborhood()?.observe(peer, custody) {
            if custody.holds() {
                self.invalidate_next_relay_holder_before(peer, false)?;
            } else {
                self.forget_next_relay_holder(Some(peer))?;
            }
            self.relay_chain_changed();
        }
        Ok(())
    }

    /// Forget what is known of `peer`'s relay custody, once it left this node's topology.
    pub(crate) fn forget_relay_custody(&self, peer: Did) -> Result<()> {
        self.lock_relay_neighborhood()?.forget(peer);
        self.lock_relay_custody_declared()?.remove(&peer);
        self.forget_next_relay_holder(Some(peer))
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

    /// The segment of the ring when this node is leaving and handing off its relay carriers
    /// (`local_holds = false`, #1057).
    pub(crate) fn relay_segment_for_leave(&self) -> Result<RelaySegment> {
        self.with_relay_view(|state, neighborhood| neighborhood.segment(state, false))
    }

    /// The segment of the ring when `leaving` is handing off its relay carriers (#1057).
    pub(crate) fn relay_segment_without(&self, leaving: Did) -> Result<RelaySegment> {
        let holds = self.holds_relay_mail();
        self.with_relay_view(|state, neighborhood| {
            neighborhood.segment_without(state, holds, leaving)
        })
    }

    /// The holder this view names for the relay key `key` (`Law (Holder)`).
    pub(crate) fn relay_holder(&self, key: Did) -> Result<RelayHolder> {
        Ok(relay_holder(&self.relay_segment()?, key))
    }

    /// Take the previous holders `previous`, nearest first, that `from` passed, iff `from` is
    /// this node's predecessor; the result says whether they were taken.
    pub(crate) fn accept_previous_relay_holder(
        &self,
        from: Did,
        previous: Vec<PreviousHolder>,
    ) -> Result<bool> {
        self.with_relay_view(|state, neighborhood| neighborhood.pass(state, from, previous))
    }

    /// The previous holders this node passes to its successor head, with that head.
    pub(crate) fn previous_relay_holder_to_pass(
        &self,
    ) -> Result<Option<(Did, Vec<PreviousHolder>)>> {
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
            .get_relay(now_ms)
            .await?
            .into_iter()
            .map(|(key, _)| key.placement())
            .filter(|key| relay_holder(&segment, *key) == RelayHolder::Unknown)
            .collect::<Vec<_>>();
        self.lock_relay_holder_hints()?.retain_unplaced(&unplaced);
        Ok(unplaced)
    }

    /// Current query key for `carrier_key`: the in-flight `asked` key if one is active, else
    /// the `stepped` key, else `carrier_key` itself.
    pub(crate) fn relay_holder_query_key(&self, carrier_key: Did) -> Result<Did> {
        let hints = self.lock_relay_holder_hints()?;
        Ok(hints
            .asked
            .get(&carrier_key)
            .map(|&(query_key, _)| query_key)
            .or_else(|| hints.stepped.get(&carrier_key).copied())
            .unwrap_or(carrier_key))
    }

    /// Clear any stepped query key and outstanding query for `carrier_key`.
    pub(crate) fn reset_relay_holder_query(&self, carrier_key: Did) -> Result<()> {
        let mut hints = self.lock_relay_holder_hints()?;
        hints.stepped.remove(&carrier_key);
        hints.asked.remove(&carrier_key);
        Ok(())
    }

    /// Record that the current stepped query key for `carrier_key` is asked for its holder
    /// (`Law (Asked)`), returning `(query_key, request_id)`.
    pub(crate) fn ask_relay_holder_query(&self, carrier_key: Did) -> Result<(Did, uuid::Uuid)> {
        Ok(self
            .lock_relay_holder_hints()?
            .ask_query(carrier_key, crate::utils::new_uuid()))
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    /// Record that the owner of `key` is asked for its holder (`Law (Asked)`), returning the
    /// single-use correlation token the answer must echo.
    pub(crate) fn ask_relay_holder(&self, key: Did) -> Result<uuid::Uuid> {
        self.ask_relay_holder_query(key).map(|(_, id)| id)
    }

    /// The peers this node knows not to chain through (#1070, #1182): peers that declared
    /// `HoldsNone` or are currently suspected.
    pub(crate) fn relay_excluded_holders(&self) -> Result<Vec<Did>> {
        let neighborhood = self.lock_relay_neighborhood()?;
        let mut excluded = neighborhood.storage_less_peers();
        excluded.extend(neighborhood.suspects());
        excluded.sort_unstable();
        excluded.dedup();
        Ok(excluded)
    }

    /// The `(holder, horizon)` this node answers as the owner of `key` when `excluded` holders
    /// are skipped (`Law (Owner places)`, #1182).
    pub(crate) fn answer_relay_holder_query(
        &self,
        key: Did,
        excluded: &[Did],
    ) -> Result<(Option<Did>, Option<Did>)> {
        let segment = self.relay_segment()?;
        let holder =
            crate::dht::relay_holder::owner_answer_excluding(&segment, self.did, key, excluded);
        let horizon = match holder {
            Some(_) => None,
            None => self.with_topology_state(|state| {
                state
                    .successors
                    .last()
                    .copied()
                    .or_else(|| excluded.contains(&self.did).then_some(self.did))
            })?,
        };
        Ok((holder, horizon))
    }

    /// Whether `origin`'s answer `(holder, horizon)` for `key` is consistent with `origin`
    /// owning `key` (`owner_answer`, `Law (Owner places)`): an owner answers itself
    /// (`holder == Some(origin)`), or names the first holder at or after `key` in its segment
    /// (`key ∈ (origin, holder]`), or answers `None` with a horizon covering
    /// `key ∈ (origin, horizon]`.
    pub(super) fn responder_answers_relay_key(
        &self,
        origin: Did,
        key: Did,
        holder: Option<Did>,
        horizon: Option<Did>,
    ) -> bool {
        origin != self.did
            && match (holder, horizon) {
                (Some(holder), None) => holder == origin || in_arc(origin, key, holder),
                (None, Some(horizon)) => horizon == origin || in_arc(origin, key, horizon),
                (None, None) => origin != key,
                (Some(_), Some(_)) => false,
            }
    }

    /// Invokes `then` iff `origin`'s answer `(holder, horizon)` is consistent with `key`
    /// (`Law (Owner places)`).
    pub(super) fn with_verified_relay_answer_and_then<T>(
        &self,
        origin: Did,
        key: Did,
        holder: Option<Did>,
        horizon: Option<Did>,
        then: impl FnOnce(Option<Did>, Option<Did>) -> Result<T>,
    ) -> Result<Option<T>> {
        self.responder_answers_relay_key(origin, key, holder, horizon)
            .then(|| then(holder, horizon))
            .transpose()
    }

    /// Take `holder`, `origin`'s answer to the outstanding query `(key, request_id)`
    /// (`Law (Asked)`): end the query iff `(key, request_id)` matches and `origin` is
    /// consistent with `key`, and record `holder` iff it is another node that holds relay
    /// mail as far as this node knows. The result says whether the hint changed, so only a
    /// new holder costs a pass.
    pub(crate) fn name_relay_holder(
        &self,
        key: Did,
        request_id: uuid::Uuid,
        origin: Did,
        holder: Option<Did>,
        horizon: Option<Did>,
    ) -> Result<bool> {
        self.with_verified_relay_answer_and_then(origin, key, holder, horizon, |holder, horizon| {
            let neighborhood = self.lock_relay_neighborhood()?;
            let verified_holder =
                holder.filter(|&candidate| candidate != self.did && neighborhood.chains(candidate));
            drop(neighborhood);
            let next_key = if verified_holder.is_none() {
                holder
                    .or(horizon)
                    .filter(|&stepped| stepped != self.did)
                    .map(|stepped| stepped + Did::from(1u32))
                    .filter(|&next_key| next_key != self.did && in_arc(key, next_key, self.did))
            } else {
                None
            };
            Ok(self.lock_relay_holder_hints()?.take_answer(
                key,
                request_id,
                verified_holder,
                next_key,
            ))
        })
        .map(|changed| changed.unwrap_or(false))
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
