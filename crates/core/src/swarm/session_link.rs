//! Delegation references on one link: what each end remembers, as pure state machines.
//!
//! A link is one admitted connection generation between two nodes. Every frame on it carries
//! two delegation slots (the origin's and the current hop's, see
//! [`WirePayload`](crate::message::WirePayload)); both ends verify both proofs, so both slots
//! matter at every hop, not only at the final destination. The few delegations behind those
//! slots repeat for the life of the link, so each direction of a link keeps one table, and the
//! two tables are the two sides of a lease:
//!
//! ```text
//!   sender   S : DelegationLessee   "what I sent inline, and the lease the peer granted each"
//!   receiver R : DelegationLessor   "what this link carried inline, under the lease I granted"
//! ```
//!
//! The link is a datagram link for all this module assumes: an accepted frame may arrive late,
//! out of order, or never. Nothing here relies on ordering.
//!
//! ```text
//!   sender:   slot s ─┬─ leased(s), t_S + Δ < until_S ─▶ Digest(s)
//!                     └─ otherwise ─────────────────────▶ Inline(s)       (s pending)
//!
//!   receiver: frame arrives ─┬─ every Digest held ─▶ resolved ─▶ verify
//!                            └─ otherwise ─────────▶ missed: dropped uncharged, Unknown(d)
//!             frame verified ─┬─ inline slot, held or room ──────────▶ lease ─▶ Known(d, r)
//!                             └─ referenced slot, < LEASE/2 left ──▶ renew ─▶ Known(d, r)
//!
//!   sender:   Known(d, r) at t_S ─▶ until_S := max(until_S, t_S + r)
//!             Unknown(d)         ─▶ d pending: inline again
//! ```
//!
//! Law (lease). The receiver grants `d` only once `d` is resident, for `r = until − t_R` where
//! `until = min(exp(d), t_R + LEASE)` on its own clock, and sends `Known(d, r)`: a duration, not
//! an instant, so no clock is compared with another. The sender, receiving it at `t_S`, holds
//! the lease until `until_S = t_S + r`, the largest such value it received. In real time,
//! whatever the two clocks read, `until_S` exceeds `until` by exactly the transit `c` of the
//! `Known` it came from, and every value counts from its own grant, so a stale `Known` never
//! lengthens a newer one. The receiver never evicts a lease before `until + G`, and refuses to
//! lease a delegation when its table is full of such leases (the sender then keeps sending it
//! inline, which is always admissible). The sender references `d` only while
//! `t_S + Δ < until_S`. So a reference of transit `t` resolves whenever `c + t ≤ Δ + G`: the
//! confirmation and the reference each within one delivery bound, on every interleaving and
//! every loss. An honest sender's reference does not miss, nothing is ever held, and the frames
//! of one lane reach the logical stage in their arrival order (#908). `until` never decreases; a verified
//! frame whose reference finds less than half a lease left renews it.
//!
//! Law (residence). A lease that ended, grace included, is reclaimed only when its slot is
//! needed for a new lease; until then a reference to it still resolves. So a frame that waits
//! in its lane at the receiver resolves however long it waited, unless the table filled up
//! meanwhile.
//!
//! Law (repair). On an honest link a miss means transits beyond `Δ + G` under a full table, or
//! a fault at this end, so it is not charged: the frame is dropped and the sender told
//! `Unknown(d)`, after which it sends `d` inline again. A dishonest sender gains nothing by
//! missing: its own frame is dropped. A stale `Unknown` that overtakes a newer `Known` costs
//! inline frames until the next lease, never a miss.
//!
//! Law (bound). The receiver's table holds at most [`LESSOR_TABLE_CAPACITY`] leases, and
//! [`LINK_PEER_RESERVE`] of them are only for the link peer's own (hop) delegations, so relayed
//! origins cannot push the peer's own session inline. The sender's table is a
//! least-recently-referenced cache of [`LESSEE_TABLE_CAPACITY`]; what it forgets travels
//! inline again. A verified frame causes at most one lease per distinct delegation it carries.
//!
//! Law (admission): the receiver grants and renews leases only for frames that verified; the
//! sender records a lease only for a digest it sent inline, on the generation it sent it on.
//! Nothing an unrelated party says reaches either table.
//!
//! Law (expiry): no frame is sent with a delegation that expires within `Δ` (the sender refuses
//! it), so an origin delegation that expired at most `G` before arrival expired in flight and
//! is not charged to the hop that carried it. The sender's table holds no expired delegation;
//! a receiver's lease never outlives its delegation, and verification refuses an expired one,
//! so expiry forces a fresh delegation and never resurrects an old one.
//!
//! Time is an argument of every step, never read here.

use std::collections::BTreeSet;
use std::collections::VecDeque;

use crate::delegation::Delegation;
use crate::delegation::DelegationDigest;
use crate::error::Error;
use crate::error::Result;
use crate::message::DelegationRef;
use crate::message::LinkControl;
use crate::message::MessagePayload;
use crate::message::PerSlot;
use crate::message::SlotEncoding;
use crate::message::WirePayload;
use crate::swarm::transport::DELIVERY_BOUND;

/// Sessions the sending end of one link remembers: its own, and the origins of the traffic it
/// forwards, least recently referenced first out, so the working set of a busy link stays
/// resident.
pub(crate) const LESSEE_TABLE_CAPACITY: usize = 64;
/// The leases the receiving end of one link grants at most: the delegations one link may pin.
pub(crate) const LESSOR_TABLE_CAPACITY: usize = 2 * LESSEE_TABLE_CAPACITY;
/// The leases reserved for the link peer's own (hop) delegations: relayed origins fill at most
/// the rest.
pub(crate) const LINK_PEER_RESERVE: usize = 8;
/// How long a lease runs from its grant or renewal, short of its delegation's expiry: how long
/// an unused delegation is guaranteed its slot in the receiver's table.
pub(crate) const LEASE_MS: u128 = 5 * 60 * 1000;
/// `Δ`: how long before a lease ends the sender stops referencing it, and how long before a
/// delegation expires the sender stops sending it at all: one delivery bound.
pub(crate) const LEASE_MARGIN_MS: u128 = DELIVERY_BOUND.as_millis();
/// `G`: how long after a lease ends the receiver still keeps it from being reclaimed: one
/// delivery bound. A reference resolves whenever its transit and its confirmation's stay
/// under `Δ + G` together.
pub(crate) const LEASE_GRACE_MS: u128 = DELIVERY_BOUND.as_millis();

/// The room each slot may fill in the receiver's table: the origin slot leaves
/// [`LINK_PEER_RESERVE`] leases to the hop slot.
const SLOT_ROOM: PerSlot<usize> = PerSlot {
    origin: LESSOR_TABLE_CAPACITY - LINK_PEER_RESERVE,
    hop: LESSOR_TABLE_CAPACITY,
};

const _: () = assert!(LINK_PEER_RESERVE < LESSOR_TABLE_CAPACITY);
const _: () = assert!(LEASE_MARGIN_MS + LEASE_GRACE_MS < LEASE_MS / 2);

/// The digests of a set of delegations: what a frame misses.
pub(crate) type Digests = BTreeSet<DelegationDigest>;

/// Whether `end_ms` falls within the margin `Δ` of `now_ms`: too soon to rely on in a frame
/// sent at `now_ms`.
const fn within_margin(end_ms: u128, now_ms: u128) -> bool {
    end_ms <= now_ms.saturating_add(LEASE_MARGIN_MS)
}

/// The signing horizon of a delegation that expires at `expires_at_ms`: `e − Δ`, the first
/// instant from which the sender (`DelegationLessee::encode`) refuses every frame that carries it.
///
/// Law (horizon): `now ≥ signing_horizon_ms(e) ⇔ within_margin(e, now)` at every instant. A
/// node's own delegation fills the hop slot of every frame it sends, so from its horizon on the
/// node is mute: no origin send, relay or stabilization leaves it, and it stays so, since its
/// delegation is fixed for the life of the process. The process shell acts on this instant
/// (#1063); this module only states it.
pub const fn signing_horizon_ms(expires_at_ms: u128) -> u128 {
    expires_at_ms.saturating_sub(LEASE_MARGIN_MS)
}

/// Whether `now_ms` is past the grace `G` after `end_ms`.
const fn past_grace(end_ms: u128, now_ms: u128) -> bool {
    now_ms > end_ms.saturating_add(LEASE_GRACE_MS)
}

/// Whether a delegation that expires at `expires_at_ms` expired at most `G` before `now_ms`:
/// what the margin `Δ` makes possible only by expiry in flight (the expiry law).
pub(crate) const fn expired_within_grace(expires_at_ms: u128, now_ms: u128) -> bool {
    expires_at_ms < now_ms && !past_grace(expires_at_ms, now_ms)
}

/// A bounded map `DelegationDigest ⇀ Delegation × A`, ordered from least to most recently
/// inserted or referenced, where `A` is what one end of the link annotates a delegation with.
///
/// Invariant: `entries.len() <= capacity`, digests are pairwise distinct, and every entry
/// satisfies `entry.digest = entry.delegation.digest()`.
#[derive(Debug)]
struct DelegationTable<A> {
    entries: VecDeque<TableEntry<A>>,
    capacity: usize,
}

/// One delegation the table holds.
#[derive(Debug)]
struct TableEntry<A> {
    digest: DelegationDigest,
    delegation: Delegation,
    annotation: A,
}

impl<A> DelegationTable<A> {
    /// The empty table that keeps at most `capacity` delegations.
    const fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity,
        }
    }

    /// The entry addressed by `digest`, if the table holds it.
    fn get(&self, digest: DelegationDigest) -> Option<&TableEntry<A>> {
        self.entries.iter().find(|entry| entry.digest == digest)
    }

    /// [`Self::get`], mutably.
    fn get_mut(&mut self, digest: DelegationDigest) -> Option<&mut TableEntry<A>> {
        self.entries.iter_mut().find(|entry| entry.digest == digest)
    }

    /// Forget every entry `stale` judges stale.
    fn evict(&mut self, stale: impl Fn(&TableEntry<A>) -> bool) {
        self.entries.retain(|entry| !stale(entry));
    }

    /// The delegations currently held.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The sender's order: least recently referenced first out.
impl DelegationTable<Option<Lease>> {
    /// The entry `digest` addresses, now the most recently referenced, if the table holds it.
    fn touch(&mut self, digest: DelegationDigest) -> Option<&TableEntry<Option<Lease>>> {
        let position = self
            .entries
            .iter()
            .position(|entry| entry.digest == digest)?;
        let entry = self.entries.remove(position)?;
        self.entries.push_back(entry);
        self.entries.back()
    }

    /// Hold `delegation` under `digest` as the most recently referenced entry, with no lease,
    /// evicting the least recently referenced entry when full.
    ///
    /// Pre: `digest = delegation.digest()`, and the table does not hold `digest`.
    fn insert_evicting_oldest(&mut self, digest: DelegationDigest, delegation: Delegation) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(TableEntry {
            digest,
            delegation,
            annotation: None,
        });
    }

    /// Forget everything.
    fn clear(&mut self) {
        self.entries.clear();
    }
}

/// The receiver's admission: a lease is never evicted before its grace ends.
impl DelegationTable<Lease> {
    /// Hold `delegation` under `digest` with `lease` if fewer than `room` entries are held once
    /// every lease past its grace at `now_ms` is reclaimed. Post: `true` iff it was added.
    ///
    /// Pre: `digest = delegation.digest()`, the table does not hold `digest`, and
    /// `room <= capacity`.
    fn insert_within(
        &mut self,
        room: usize,
        digest: DelegationDigest,
        delegation: Delegation,
        lease: Lease,
        now_ms: u128,
    ) -> bool {
        if self.entries.len() >= room {
            self.evict(|entry| entry.annotation.is_reclaimable_at(now_ms));
        }
        if self.entries.len() >= room {
            return false;
        }
        self.entries.push_back(TableEntry {
            digest,
            delegation,
            annotation: lease,
        });
        true
    }
}

/// A lease on one delegation: its end, on the clock of the end that holds the record.
///
/// Leases form a join-semilattice under [`Lease::join`] (the longer lease): `join` is
/// idempotent, commutative and associative, so the order in which grants and renewals arrive
/// never matters, and a stale one never shortens the lease held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Lease {
    until_ms: u128,
}

impl Lease {
    /// The lease the receiver grants at `now_ms` on `delegation`: [`LEASE_MS`], short of its
    /// expiry.
    fn granted(delegation: &Delegation, now_ms: u128) -> Self {
        Self {
            until_ms: delegation
                .expires_at_ms()
                .min(now_ms.saturating_add(LEASE_MS)),
        }
    }

    /// The lease the sender records on receiving, at `now_ms`, a grant of `remaining_ms`.
    fn received(remaining_ms: u128, now_ms: u128) -> Self {
        Self {
            until_ms: now_ms.saturating_add(remaining_ms),
        }
    }

    /// The longer of two leases.
    fn join(self, other: Self) -> Self {
        Self {
            until_ms: self.until_ms.max(other.until_ms),
        }
    }

    /// What is left of the lease at `now_ms`.
    const fn remaining_at(self, now_ms: u128) -> u128 {
        self.until_ms.saturating_sub(now_ms)
    }

    /// Whether a frame encoded at `now_ms` may reference the delegation under this lease.
    const fn references_at(self, now_ms: u128) -> bool {
        !within_margin(self.until_ms, now_ms)
    }

    /// Whether the receiver may reclaim this lease's slot at `now_ms`.
    const fn is_reclaimable_at(self, now_ms: u128) -> bool {
        past_grace(self.until_ms, now_ms)
    }

    /// Whether a reference at `now_ms` finds less than half a lease left.
    const fn wants_renewal_at(self, now_ms: u128) -> bool {
        self.remaining_at(now_ms) < LEASE_MS / 2
    }
}

/// The sending end of one link: the delegations it has sent inline, and the lease the peer
/// granted each, if any.
///
/// ```text
///   encode  : S × Generation × Payload × Time → S × PerSlot<DelegationRef>
///   observe : S × Generation × LinkControl × Time → S
/// ```
#[derive(Debug)]
pub(crate) struct DelegationLessee {
    /// The connection generation the table belongs to; `None` until the first frame.
    generation: Option<u64>,
    sent: DelegationTable<Option<Lease>>,
}

impl DelegationLessee {
    /// The sender state of a link that has carried nothing.
    pub(crate) const fn new() -> Self {
        Self {
            generation: None,
            sent: DelegationTable::new(LESSEE_TABLE_CAPACITY),
        }
    }

    /// Whether the table is `generation`'s: what another generation sent was sent on another
    /// link.
    fn is_current(&self, generation: u64) -> bool {
        self.generation == Some(generation)
    }

    /// Make the table the one of `generation`: a newer generation is a new link with an empty
    /// table, an older one is a link that no longer exists. Post: `true` iff `generation` is
    /// the current one afterwards.
    fn enter(&mut self, generation: u64) -> bool {
        if self.generation.is_none_or(|current| generation > current) {
            self.generation = Some(generation);
            self.sent.clear();
        }
        self.is_current(generation)
    }

    /// Decide how each slot of `payload` travels on `generation` at `now_ms`, and remember
    /// what was sent inline.
    ///
    /// A slot whose delegation the peer leased, with more than `Δ` of the lease left, travels by
    /// digest; every other slot travels inline, and the delegation enters the table, unleased,
    /// if it was not held. Refused with [`Error::DelegationExpiresWithinMargin`] when a slot's
    /// delegation expires within `Δ`: such a frame could need its delegation past its expiry in
    /// flight.
    pub(crate) fn encode<'a>(
        &mut self,
        generation: u64,
        payload: &'a MessagePayload,
        now_ms: u128,
    ) -> Result<PerSlot<DelegationRef<'a>>> {
        let delegations = payload.delegations();
        let expiring = delegations
            .into_array()
            .into_iter()
            .any(|delegation| within_margin(delegation.expires_at_ms(), now_ms));
        if expiring {
            return Err(Error::DelegationExpiresWithinMargin);
        }
        if !self.enter(generation) {
            return Ok(delegations.map(DelegationRef::inline));
        }
        self.sent
            .evict(|entry| entry.delegation.is_expired_at(now_ms));
        Ok(PerSlot {
            origin: self.encode_slot(delegations.origin, now_ms)?,
            hop: self.encode_slot(delegations.hop, now_ms)?,
        })
    }

    /// [`Self::encode`] for one slot of the current generation.
    fn encode_slot<'a>(
        &mut self,
        delegation: &'a Delegation,
        now_ms: u128,
    ) -> Result<DelegationRef<'a>> {
        let digest = delegation.digest()?;
        match self.sent.touch(digest) {
            Some(entry)
                if entry
                    .annotation
                    .is_some_and(|lease| lease.references_at(now_ms)) =>
            {
                Ok(DelegationRef::Digest(digest))
            }
            Some(_) => Ok(DelegationRef::inline(delegation)),
            None => {
                self.sent.insert_evicting_oldest(digest, delegation.clone());
                Ok(DelegationRef::inline(delegation))
            }
        }
    }

    /// Learn what the peer said, at `now_ms`, about the link of `generation`:
    ///
    /// ```text
    ///   Known(d, r) ─▶ lease(d) := lease(d) ⊔ (now + r)
    ///   Unknown(d)  ─▶ lease(d) := none: d travels inline again
    /// ```
    ///
    /// A control frame about a digest this end never sent inline, or sent on another
    /// generation, changes nothing.
    pub(crate) fn observe(&mut self, generation: u64, control: &LinkControl, now_ms: u128) {
        if !self.is_current(generation) {
            return;
        }
        let Some(entry) = self.sent.get_mut(control.digest()) else {
            return;
        };
        entry.annotation = match *control {
            LinkControl::Known { remaining_ms, .. } => {
                let received = Lease::received(remaining_ms, now_ms);
                Some(
                    entry
                        .annotation
                        .map_or(received, |held| held.join(received)),
                )
            }
            LinkControl::Unknown(_) => None,
        };
    }
}

/// A lease to tell the sender about: `Known(digest, remaining)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Confirmation {
    /// The delegation leased.
    pub(crate) digest: DelegationDigest,
    /// What is left of the lease when it was granted or renewed.
    pub(crate) remaining_ms: u128,
}

impl From<Confirmation> for LinkControl {
    fn from(confirmation: Confirmation) -> Self {
        Self::Known {
            digest: confirmation.digest,
            remaining_ms: confirmation.remaining_ms,
        }
    }
}

/// A frame the receiver resolved: the self-contained payload, not yet verified, and how each
/// slot travelled, which is what [`DelegationLessor::admit_verified`] may learn from it.
pub(crate) struct ResolvedFrame {
    /// The self-contained payload; not yet verified.
    pub(crate) payload: MessagePayload,
    /// How each slot travelled.
    pub(crate) encoding: PerSlot<SlotEncoding>,
}

/// What became of a frame on a link: resolved, or missed with the digests the table lacks.
pub(crate) enum Resolution {
    /// Every reference resolved.
    Resolved(Box<ResolvedFrame>),
    /// The frame referenced these digests and the table holds none of them: it is dropped,
    /// uncharged, and the sender told about each (the repair law).
    Missed(Digests),
}

/// The receiving end of one link: the delegations it has carried inline, each under the lease
/// this end granted. See the module documentation for the laws.
pub(crate) struct DelegationLessor {
    leases: DelegationTable<Lease>,
}

impl DelegationLessor {
    /// The receiver state of a link that has carried nothing.
    pub(crate) const fn new() -> Self {
        Self {
            leases: DelegationTable::new(LESSOR_TABLE_CAPACITY),
        }
    }

    /// Resolve `frame` against the table. Residence alone decides (the residence law): when
    /// the frame arrived, or how long it waited since, does not.
    pub(crate) fn resolve(&self, frame: Box<WirePayload<'static>>) -> Resolution {
        let missing: Digests = frame
            .delegation_refs()
            .into_array()
            .into_iter()
            .filter_map(DelegationRef::digest)
            .filter(|digest| self.leases.get(*digest).is_none())
            .collect();
        if !missing.is_empty() {
            return Resolution::Missed(missing);
        }
        let encoding = frame.delegation_refs().map(DelegationRef::encoding);
        let resolved = frame.resolve(|delegation| match delegation {
            DelegationRef::Inline(delegation) => Ok(delegation.into_owned()),
            DelegationRef::Digest(digest) => self
                .leases
                .get(digest)
                .map(|entry| entry.delegation.clone())
                .ok_or(digest),
        });
        match resolved {
            Ok(payload) => Resolution::Resolved(Box::new(ResolvedFrame { payload, encoding })),
            Err(digest) => Resolution::Missed(Digests::from([digest])),
        }
    }

    /// Learn from a frame that verified at `now_ms`, and name the leases to confirm to the
    /// peer, at most one per distinct delegation:
    ///
    /// ```text
    ///   inline slot      ─▶ lease(d) := lease(d) ⊔ granted(now), or a new lease if there is room
    ///   referenced slot  ─▶ lease(d) := granted(now) if less than half of it is left
    /// ```
    ///
    /// So a sender that keeps sending a leased delegation inline (its confirmation lost or
    /// still in flight, or its lease ended) is confirmed again. A delegation that finds no room
    /// is not leased and not confirmed: it keeps travelling inline. The origin slot leaves
    /// [`LINK_PEER_RESERVE`] leases to the hop slot.
    ///
    /// Pre: `payload` verified at `now_ms`, so each of its delegations is a live, authorized
    /// delegation; `encoding` is the [`ResolvedFrame::encoding`] it was resolved with.
    pub(crate) fn admit_verified(
        &mut self,
        payload: &MessagePayload,
        encoding: PerSlot<SlotEncoding>,
        now_ms: u128,
    ) -> Result<Vec<Confirmation>> {
        let delegations = payload.delegations();
        // A peer's own frame carries one delegation in both slots: learn it once, as the hop's.
        let slots = delegations.zip(encoding).zip(SLOT_ROOM).into_array();
        let distinct = if delegations.is_diagonal() {
            &slots[1..]
        } else {
            &slots[..]
        };
        let mut confirm = Vec::with_capacity(distinct.len());
        for &((delegation, encoding), room) in distinct {
            let digest = delegation.digest()?;
            let granted = Lease::granted(delegation, now_ms);
            let lease = match (encoding, self.leases.get_mut(digest)) {
                (SlotEncoding::Inline, Some(entry)) => {
                    entry.annotation = entry.annotation.join(granted);
                    Some(entry.annotation)
                }
                (SlotEncoding::Inline, None) => self
                    .leases
                    .insert_within(room, digest, delegation.clone(), granted, now_ms)
                    .then_some(granted),
                (SlotEncoding::Referenced, Some(entry)) => {
                    let renewed = entry.annotation.join(granted);
                    let renews =
                        entry.annotation.wants_renewal_at(now_ms) && renewed != entry.annotation;
                    renews.then(|| {
                        entry.annotation = renewed;
                        renewed
                    })
                }
                (SlotEncoding::Referenced, None) => None,
            };
            if let Some(lease) = lease {
                confirm.push(Confirmation {
                    digest,
                    remaining_ms: lease.remaining_at(now_ms),
                });
            }
        }
        Ok(confirm)
    }

    /// The delegations currently leased.
    #[cfg(test)]
    pub(crate) fn leased_len(&self) -> usize {
        self.leases.len()
    }
}

#[cfg(test)]
mod test_session_link;
