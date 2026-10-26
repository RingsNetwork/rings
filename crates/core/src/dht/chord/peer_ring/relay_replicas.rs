//! Replicated custody of a [`PeerRing`] (#777; durable-records, Section X): the members its
//! view names for a relay key, its place in a key's chain, which copies it admits, the next
//! member it resolves past its view, and the holds it forwards down a chain until the rest of
//! the chain holds them.
//!
//! ```text
//! Law (Chain replication) a hold goes to h₀; each member forwards it to its next member and
//!                         acknowledges the member it came from once the rest of the chain did;
//!                         the last member acknowledges at once, and h₀ acknowledges the writer
//! Law (Replica admission) a node takes a copy of the carrier at k from s iff its own view makes
//!                         s its previous or its next member of k
//! next member             in the successor list, or past it n₁, the first holder after this
//!                         node, resolved by the owner query and cached until the successor list
//!                         changes or the holder is forgotten: one query serves every key
//! ```
//!
//! A replication is recomputed on the view at every step, so a member that left stops being
//! forwarded to. At most [`UNACKNOWLEDGED_RELAY_HOLDS_MAX`] replications are tracked: a hold
//! taken beyond it is not forwarded and not acknowledged, and its writer sends it again, which
//! tries again. The locks of the replications and of the next holder are leaves of
//! `Law (Lock order)`.

use std::sync::MutexGuard;

use super::PeerRing;
use super::RelayReplications;
use crate::consts::UNACKNOWLEDGED_RELAY_HOLDS_MAX;
use crate::dht::entry::EntryOperation;
use crate::dht::relay_holder::chain_position;
use crate::dht::relay_holder::relay_members;
use crate::dht::relay_holder::ChainNext;
use crate::dht::relay_holder::ChainPosition;
use crate::dht::relay_holder::RelayMembers;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::RelayHoldDigest;

/// A hold this node took and forwards down the chain of its key, while the rest of the chain
/// has not acknowledged it.
#[derive(Debug)]
pub(crate) struct PendingReplication {
    /// The hold as its writer stamped it, forwarded byte for byte.
    hold: EntryOperation,
    /// The node it came from: the writer at the holder, the previous member at a member.
    upstream: Did,
    /// The next member it was last forwarded to, `None` while the next member is unresolved.
    downstream: Option<Did>,
}

/// The first holder after this node past its successor list, as the owner query resolved it.
#[derive(Debug, Default)]
pub(crate) struct NextHolder {
    /// The resolved holder, valid until the successor list changes or it is forgotten.
    holder: Option<Did>,
    /// The key whose owner is asked and has not answered yet (`Law (Asked)`).
    asked: Option<Did>,
}

/// What this node does next with a hold it took, as the holder or a member of its key.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ReplicationStep {
    /// This node ends the chain: acknowledge upstream at once.
    Held,
    /// Forward it to the next member; with its bytes iff `bytes`, else its digest alone, the
    /// next member having been sent it before.
    Forward {
        /// The next member.
        to: Did,
        /// Whether the forward carries the hold's bytes.
        bytes: bool,
    },
    /// The next member lies past the view: ask the owner of `key` (`FindRelayHolder`).
    Ask {
        /// The key past the successor list to ask about.
        key: Did,
    },
    /// The next member is being resolved, or the hold is not tracked: wait for a resend.
    Wait,
}

/// What an acknowledgement from a next member does.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ReplicaAcknowledgement {
    /// This node forwards no such hold to that node: the acknowledgement is a writer's
    /// (`Law (Acknowledged)`).
    NotReplicated,
    /// The rest of the chain holds it: acknowledge `upstream`.
    Held {
        /// The writer, or the previous member.
        upstream: Did,
    },
}

impl PeerRing {
    /// The members this view names for the relay key `key` (`Law (Members)`).
    pub(crate) fn relay_members(&self, key: Did) -> Result<RelayMembers> {
        Ok(relay_members(
            &self.relay_segment()?,
            key,
            self.relay_replicas(),
        ))
    }

    /// Whether this node is a member of the relay key `key` under its own view.
    pub(crate) fn is_relay_member(&self, key: Did) -> Result<bool> {
        Ok(self.relay_members(key)?.contains(self.did))
    }

    /// This node's place in the chain of `key`, `None` when it is no member.
    pub(crate) fn relay_chain_position(&self, key: Did) -> Result<Option<ChainPosition>> {
        Ok(chain_position(
            &self.relay_segment()?,
            key,
            self.did,
            self.relay_replicas(),
        ))
    }

    /// The next member of `position`: `None` at the chain's end, or while it lies past the view
    /// unresolved.
    fn resolved_next_member(&self, position: ChainPosition) -> Result<Option<Did>> {
        Ok(match position.next {
            ChainNext::End => None,
            ChainNext::Member(next) => Some(next),
            ChainNext::BeyondView => self.lock_relay_next_holder()?.holder,
        })
    }

    /// Whether the chain of `key` ends at this node, of rank `position.rank`, although its view
    /// does not show it: the next holder resolved past the view is this node, or a member before
    /// it, so the ring holds fewer holders than `r` and the chain came round.
    fn chain_wraps(&self, key: Did, position: ChainPosition, next: Option<Did>) -> Result<bool> {
        let Some(next) = next else {
            return Ok(false);
        };
        if next == self.did {
            return Ok(true);
        }
        Ok(match self.relay_members(key)? {
            RelayMembers::Members(members) => members
                .iter()
                .take(position.rank)
                .any(|member| *member == next),
            RelayMembers::Unheld | RelayMembers::Unknown => false,
        })
    }

    /// Whether this node takes a copy of the carrier at `key` from `sender` (`Law (Replica
    /// admission)`): its own view makes `sender` its previous or its next member of `key`.
    pub(crate) fn admits_relay_copy(&self, key: Did, sender: Did) -> Result<bool> {
        let Some(position) = self.relay_chain_position(key)? else {
            return Ok(false);
        };
        let next = self.resolved_next_member(position)?;
        Ok(sender != self.did
            && (position.previous == Some(sender)
                || (next == Some(sender) && !self.chain_wraps(key, position, next)?)))
    }

    /// The next member of `key` for this node at `position`, `None` at the chain's end, while
    /// unresolved past the view, or when the chain comes round to a member before it.
    pub(crate) fn relay_chain_next(
        &self,
        key: Did,
        position: ChainPosition,
    ) -> Result<Option<Did>> {
        let next = self.resolved_next_member(position)?;
        if self.chain_wraps(key, position, next)? {
            return Ok(None);
        }
        Ok(next)
    }

    /// Whether `sender` is this node's previous member of `key`, the only node a forwarded hold
    /// is taken from (`Law (Chain replication)`).
    pub(crate) fn is_previous_relay_member(&self, key: Did, sender: Did) -> Result<bool> {
        Ok(self
            .relay_chain_position(key)?
            .is_some_and(|position| position.previous == Some(sender)))
    }

    /// The chain neighbours of this node, `p₁` and `n₁` (`Law (Chain repair)`): its nearest
    /// previous holder, and the first holder after it, in its successor list or resolved past
    /// it. Every carrier this node keeps as a member has these as its previous and next member,
    /// but at the ends of the chain.
    pub(crate) fn relay_chain_neighbours(&self) -> Result<(Option<Did>, Option<Did>)> {
        let (previous, next) = self.with_relay_view(|state, neighborhood| {
            (
                neighborhood.nearest_previous_holder(state),
                state
                    .successors
                    .iter()
                    .copied()
                    .find(|successor| neighborhood.chains(*successor)),
            )
        })?;
        let next = match next {
            Some(next) => Some(next),
            None => self.lock_relay_next_holder()?.holder,
        };
        Ok((previous, next))
    }

    /// Lock the replications, a leaf of `Law (Lock order)`.
    fn lock_relay_replications(&self) -> Result<MutexGuard<'_, RelayReplications>> {
        self.relay_replications
            .lock()
            .map_err(|_| Error::LockPoisoned)
    }

    /// Lock the next holder, a leaf of `Law (Lock order)`.
    fn lock_relay_next_holder(&self) -> Result<MutexGuard<'_, NextHolder>> {
        self.relay_next_holder
            .lock()
            .map_err(|_| Error::LockPoisoned)
    }

    /// Replicate `hold`, of digest `digest`, which this node took from `upstream` as the holder
    /// or a member of its key (`Law (Chain replication)`); called again on every resend.
    ///
    /// Post: `Held` iff this node ends the chain; `Forward` names the next member, with the
    /// bytes iff it was not forwarded there before, and tracks the replication; `Ask` names the
    /// key to ask the owner of when the next member lies past the view and no query is
    /// outstanding; `Wait` when this node is no member, the next member is being resolved, or
    /// [`UNACKNOWLEDGED_RELAY_HOLDS_MAX`] replications are tracked already.
    pub(crate) fn replicate_relay_hold(
        &self,
        digest: RelayHoldDigest,
        hold: EntryOperation,
        upstream: Did,
    ) -> Result<ReplicationStep> {
        let key = hold.entry().did;
        let Some(position) = self.relay_chain_position(key)? else {
            return Ok(ReplicationStep::Wait);
        };
        let next = self.resolved_next_member(position)?;
        if position.next == ChainNext::End || self.chain_wraps(key, position, next)? {
            self.lock_relay_replications()?.remove(&digest);
            return Ok(ReplicationStep::Held);
        }
        let mut replications = self.lock_relay_replications()?;
        let known = replications
            .get(&digest)
            .and_then(|pending| pending.downstream);
        if !replications.contains_key(&digest)
            && replications.len() >= UNACKNOWLEDGED_RELAY_HOLDS_MAX
        {
            return Ok(ReplicationStep::Wait);
        }
        replications.insert(digest, PendingReplication {
            hold,
            upstream,
            downstream: next,
        });
        drop(replications);
        match next {
            Some(to) => Ok(ReplicationStep::Forward {
                to,
                bytes: known != Some(to),
            }),
            None => self.ask_next_relay_holder(),
        }
    }

    /// Ask for the first holder past the successor list (`Law (Asked)`): the owner of the key
    /// just past its last node answers the first holder at or after it.
    ///
    /// Post: `Ask` with that key iff no query is outstanding; `Wait` otherwise, or without a
    /// successor.
    fn ask_next_relay_holder(&self) -> Result<ReplicationStep> {
        let Some(last) = self.with_topology_state(|state| state.successors.last().copied())? else {
            return Ok(ReplicationStep::Wait);
        };
        let mut next = self.lock_relay_next_holder()?;
        if next.asked.is_some() {
            return Ok(ReplicationStep::Wait);
        }
        let key = last + Did::from(1u32);
        next.asked = Some(key);
        Ok(ReplicationStep::Ask { key })
    }

    /// Take the owner's answer about `key` to this node's outstanding query for its next holder
    /// (`Law (Asked)`): the holder it names, which is cached, or, when it names none, the last
    /// node it places, past which the query is asked again.
    ///
    /// Post: `None` for an answer nobody asked for; `Some(key′)` iff the query is to be asked
    /// again for `key′`; a named holder this node knows to hold no relay mail is not taken.
    pub(crate) fn take_next_relay_holder(
        &self,
        key: Did,
        holder: Option<Did>,
        horizon: Option<Did>,
    ) -> Result<Option<Did>> {
        let named = match holder {
            Some(holder) if holder != self.did && self.lock_relay_neighborhood()?.holds(holder) => {
                Some(holder)
            }
            _ => None,
        };
        let mut next = self.lock_relay_next_holder()?;
        if next.asked != Some(key) {
            return Ok(None);
        }
        if named.is_some() {
            next.asked = None;
            next.holder = named;
            return Ok(None);
        }
        next.asked = horizon
            .filter(|horizon| *horizon != self.did)
            .map(|horizon| horizon + Did::from(1u32));
        Ok(next.asked)
    }

    /// The key of this node's outstanding query for its next holder, to ask again each pass
    /// until it is answered.
    pub(crate) fn outstanding_next_relay_holder_query(&self) -> Result<Option<Did>> {
        Ok(self.lock_relay_next_holder()?.asked)
    }

    /// Whether this node asks the owner of `key` for its next holder: an answer about `key` is
    /// the chain's, not a keeper's hint.
    pub(crate) fn asks_next_relay_holder(&self, key: Did) -> Result<bool> {
        Ok(self.lock_relay_next_holder()?.asked == Some(key))
    }

    /// Forget the resolved next holder when it is `peer`, which left, or whatever it is with
    /// `None`, the successor list having changed.
    pub(crate) fn forget_next_relay_holder(&self, peer: Option<Did>) -> Result<()> {
        let mut next = self.lock_relay_next_holder()?;
        if peer.is_none() || next.holder == peer {
            next.holder = None;
        }
        Ok(())
    }

    /// The holds this node forwards whose next member was unresolved, with the node each came
    /// from: to forward once the next member is resolved.
    pub(crate) fn unforwarded_relay_holds(&self) -> Result<Vec<(EntryOperation, Did)>> {
        Ok(self
            .lock_relay_replications()?
            .values()
            .filter(|pending| pending.downstream.is_none())
            .map(|pending| (pending.hold.clone(), pending.upstream))
            .collect())
    }

    /// Take `member`'s acknowledgement of the hold `digest` (`Law (Chain replication)`).
    ///
    /// Post: `Held { upstream }` iff `member` is the next member this node last forwarded it to,
    /// and the replication is forgotten; `NotReplicated` otherwise.
    pub(crate) fn acknowledge_relay_replica(
        &self,
        digest: RelayHoldDigest,
        member: Did,
    ) -> Result<ReplicaAcknowledgement> {
        let mut replications = self.lock_relay_replications()?;
        let forwarded = replications
            .get(&digest)
            .is_some_and(|pending| pending.downstream == Some(member));
        Ok(match replications.remove(&digest) {
            Some(pending) if forwarded => ReplicaAcknowledgement::Held {
                upstream: pending.upstream,
            },
            Some(pending) => {
                replications.insert(digest, pending);
                ReplicaAcknowledgement::NotReplicated
            }
            None => ReplicaAcknowledgement::NotReplicated,
        })
    }

    /// The hold `digest`, iff this node forwarded it to `member`, which asks for its bytes
    /// (`RelayCarrierRefused` of a forward that carried the digest alone).
    pub(crate) fn refused_relay_replica(
        &self,
        digest: RelayHoldDigest,
        member: Did,
    ) -> Result<Option<EntryOperation>> {
        Ok(self
            .lock_relay_replications()?
            .get(&digest)
            .filter(|pending| pending.downstream == Some(member))
            .map(|pending| pending.hold.clone()))
    }

    /// The hold `digest`, iff this node forwards it: a forward of its digest alone is known.
    pub(crate) fn replicated_relay_hold(
        &self,
        digest: RelayHoldDigest,
    ) -> Result<Option<EntryOperation>> {
        Ok(self
            .lock_relay_replications()?
            .get(&digest)
            .map(|pending| pending.hold.clone()))
    }

    /// Suspect `peer`, a chain neighbour whose probe went unanswered for the chain timeout
    /// (#1070): the chain moves past it, and the change runs the chain exchange at once, as a
    /// loss does (`Law (Event-driven repair)`). Nothing is evicted.
    pub(crate) fn suspect_relay_chain_neighbour(&self, peer: Did) -> Result<()> {
        if self.lock_relay_neighborhood()?.suspect(peer) {
            self.forget_next_relay_holder(Some(peer))?;
            self.relay_chain_changed();
        }
        Ok(())
    }

    /// Record `peer`'s probe answer, `on_time` within the chain timeout: a suspect cleared by
    /// enough on-time answers in a row (`Law (Clearing)`) rejoins the chains it is a member of,
    /// and the change runs the chain exchange at once, restoring its copies.
    pub(crate) fn observe_chain_neighbour_answer(&self, peer: Did, on_time: bool) -> Result<()> {
        if self
            .lock_relay_neighborhood()?
            .observe_answer(peer, on_time)
        {
            self.relay_chain_changed();
        }
        Ok(())
    }

    /// The chain changes recorded so far: losses, suspicions and clearings.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn relay_chain_changes_for_test(&self) -> u64 {
        self.relay_chain_losses.current()
    }

    /// The peers this node suspects for relay custody (#1070): probed on the chain period until
    /// their on-time answers clear them, so `Law (Clearing)` runs on the schedule it states.
    pub(crate) fn relay_suspects(&self) -> Result<Vec<Did>> {
        Ok(self.lock_relay_neighborhood()?.suspects().collect())
    }

    /// Whether `peer` is suspected for relay custody (#1070).
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn is_relay_suspected(&self, peer: Did) -> Result<bool> {
        Ok(!self.lock_relay_neighborhood()?.chains(peer))
    }

    /// Record a change of this node's chains (a chain neighbour lost, suspected, or cleared)
    /// for the chain repair task: it runs the exchange at once (`Law (Event-driven repair)`).
    pub(crate) fn relay_chain_changed(&self) {
        self.relay_chain_repair_due
            .store(true, std::sync::atomic::Ordering::Release);
        self.relay_chain_losses.advance();
    }

    /// Register a listener for the next loss of a chain neighbour (`Law (Event-driven repair)`).
    ///
    /// Pre: the caller checks [`Self::take_relay_chain_repair_due`] after this call and awaits
    /// the listener only when it answers `false` (`Law (Wake)`).
    pub(crate) fn listen_relay_chain_loss(&self) -> event_listener::EventListener {
        self.relay_chain_losses.listen()
    }

    /// Whether a chain neighbour was lost since the last call, clearing it: the chain exchange
    /// is due at once.
    pub(crate) fn take_relay_chain_repair_due(&self) -> bool {
        self.relay_chain_repair_due
            .swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// Forget every replication whose hold no member takes any more: its sender proof expired
    /// by `now_ms`, so its writer forgets it too.
    pub(crate) fn prune_relay_replications(&self, now_ms: u128) -> Result<()> {
        self.lock_relay_replications()?
            .retain(|_, pending| pending.hold.hold_is_live_at(now_ms));
        Ok(())
    }
}
