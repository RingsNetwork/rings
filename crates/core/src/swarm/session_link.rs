//! Delegation references on one link: what each end remembers, as pure state machines.
//!
//! A link is one admitted connection generation between two nodes. Every frame on it carries
//! two delegation slots (the origin's and the current hop's, see
//! [`WirePayload`](crate::message::WirePayload)); both ends verify both proofs, so both slots
//! matter at every hop, not only at the final destination. The few delegations behind those
//! slots repeat for the life of the link, so each direction of a link keeps one table:
//!
//! ```text
//!   sender   S : AnnouncedDelegations    "delegations I sent inline, and the lease the peer granted each"
//!   receiver R : ReferencedDelegations   "delegations this link carried inline, each under a lease I granted"
//! ```
//!
//! The link is a datagram link for all this module assumes: an accepted frame may arrive late,
//! out of order, or never. Nothing here relies on ordering.
//!
//! ```text
//!   sender:   slot s ─┬─ leased(s) at t_S + Δ < until_S ─▶ Digest(s)
//!                     └─ otherwise ──────────────────────▶ Inline(s)      (admit s as pending)
//!
//!   receiver: frame arrives ─┬─ every Digest leased at t_R ≤ until + G ─▶ resolved ─▶ verify
//!                            └─ otherwise ─────────────────────────────▶ missed: dropped
//!                                                                         uncharged, Unknown(d)
//!             frame verified ─▶ per inline slot: leased, or room ─▶ lease ─▶ Known(d, until)
//!
//!   sender:   Known(d, until) ─▶ until_S := max(until_S, until)
//!             Unknown(d)      ─▶ d pending: inline again
//! ```
//!
//! Law (lease). The receiver confirms `d` only once `d` is resident, with
//! `Known(d, until)`, `until = min(exp(d), t_R + LEASE)` on its own clock, and keeps `d` until
//! `until + G`: it never evicts a live lease to make room, and refuses to lease a delegation
//! when its table is full of live leases (the sender then keeps sending it inline, which is
//! always admissible). `until` never decreases; a reference that finds less than half a lease
//! left renews it. The sender references `d` only while `t_S + Δ < until_S`, the largest
//! `until` it was sent. Since the sender only learns values the receiver granted, and a stale
//! confirmation carries an older, smaller `until`, `until_S ≤ until_R`: a reference resolves
//! whenever its transit plus the two clocks' skew stays under `Δ + G`, on every interleaving
//! and every loss. So an honest sender's reference never misses, nothing is ever held, and
//! the frames of one lane reach the logical stage in their arrival order (#908).
//!
//! Law (repair). A miss is this end's clock or fault on an honest link, so it is not charged:
//! the frame is dropped and the sender told `Unknown(d)`, after which it sends `d` inline
//! again. A dishonest sender gains nothing by missing: its own frame is dropped.
//!
//! Law (bound). The receiver's table holds at most [`REFERENCED_TABLE_CAPACITY`] leases, each
//! for at most `LEASE + G` after its last use, and [`LINK_PEER_RESERVE`] of them are only for
//! the link peer's own (hop) delegations, so relayed origins cannot push the peer's own session
//! inline. The sender's table is a least-recently-referenced cache of
//! [`ANNOUNCED_TABLE_CAPACITY`]; what it forgets travels inline again.
//!
//! Law (admission): the receiver leases only delegations of frames that verified; the sender
//! leases only digests it announced, on the generation it announced them on. Nothing an
//! unrelated party says reaches either table.
//!
//! Law (expiry): no frame is sent with a delegation that expires within `Δ` (the sender refuses
//! it, so a frame never needs a delegation past its expiry in flight), and an expired
//! delegation is absent from both tables: expiry forces a fresh delegation and never
//! resurrects an old one.
//!
//! Time is an argument of every step, never read here.

use std::collections::BTreeSet;
use std::collections::VecDeque;

use rings_transport::core::transport::IRREVOCABLE_SEND_COMPLETION_TIMEOUT;

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

/// Sessions the sending end of one link remembers: its own, and the origins of the traffic it
/// forwards, least recently referenced first out, so the working set of a busy link stays
/// resident.
pub(crate) const ANNOUNCED_TABLE_CAPACITY: usize = 64;
/// The leases the receiving end of one link grants at most: the delegations one link may pin,
/// each for at most `LEASE_MS + LEASE_GRACE_MS` after its last use.
pub(crate) const REFERENCED_TABLE_CAPACITY: usize = 2 * ANNOUNCED_TABLE_CAPACITY;
/// The leases reserved for the link peer's own (hop) delegations: relayed origins fill at most
/// the rest.
pub(crate) const LINK_PEER_RESERVE: usize = 8;
/// How long a lease runs from its grant or renewal, short of its delegation's expiry: what an
/// unused delegation pins of the receiver's table.
pub(crate) const LEASE_MS: u128 = 5 * 60 * 1000;
/// `Δ`: how long before a lease ends the sender stops referencing it, and how long before a
/// delegation expires the sender stops sending it at all: one delivery bound.
pub(crate) const LEASE_MARGIN_MS: u128 = IRREVOCABLE_SEND_COMPLETION_TIMEOUT.as_millis();
/// `G`: how long after a lease ends the receiver still resolves it: one delivery bound. A
/// reference resolves whenever its transit plus the clocks' skew stays under `Δ + G`.
pub(crate) const LEASE_GRACE_MS: u128 = IRREVOCABLE_SEND_COMPLETION_TIMEOUT.as_millis();

const _: () = assert!(LINK_PEER_RESERVE < REFERENCED_TABLE_CAPACITY);
const _: () = assert!(LEASE_MARGIN_MS + LEASE_GRACE_MS < LEASE_MS / 2);

/// The digests of a set of delegations: what a frame misses.
pub(crate) type Digests = BTreeSet<DelegationDigest>;

/// A bounded map `DelegationDigest ⇀ Delegation × A`, ordered from least to most recently
/// referenced, where `A` is what one end of the link annotates a delegation with. The sender
/// keeps the order (it evicts the least recently referenced); the receiver ignores it.
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

    /// Record a reference to `digest`: it becomes the most recently referenced entry. Post:
    /// `true` iff the table holds `digest`.
    fn touch(&mut self, digest: DelegationDigest) -> bool {
        let Some(position) = self.entries.iter().position(|entry| entry.digest == digest) else {
            return false;
        };
        if let Some(entry) = self.entries.remove(position) {
            self.entries.push_back(entry);
        }
        true
    }

    /// The entry `digest` addresses, now the most recently referenced, if the table holds it
    /// live at `now_ms`; an expired entry is neither returned nor touched.
    fn touch_live(&mut self, digest: DelegationDigest, now_ms: u128) -> Option<&TableEntry<A>> {
        let position = self
            .entries
            .iter()
            .position(|entry| entry.digest == digest && !entry.delegation.is_expired_at(now_ms))?;
        let entry = self.entries.remove(position)?;
        self.entries.push_back(entry);
        self.entries.back()
    }

    /// Hold the delegation `digest` addresses as the most recently referenced entry: a reference
    /// if the table holds it, else `delegation()` with `annotation`, evicting the least recently
    /// referenced entry when full. The delegation is materialised only on first sight.
    ///
    /// Pre: `digest = delegation().digest()`.
    fn admit(
        &mut self,
        digest: DelegationDigest,
        delegation: impl FnOnce() -> Delegation,
        annotation: A,
    ) {
        if self.touch(digest) {
            return;
        }
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(TableEntry {
            digest,
            delegation: delegation(),
            annotation,
        });
    }

    /// Hold `delegation` under `digest` with `annotation` if fewer than `room` entries are held,
    /// never evicting one. Post: `true` iff it was added.
    ///
    /// Pre: `digest = delegation.digest()`, and the table does not hold `digest`.
    fn insert_within(
        &mut self,
        room: usize,
        digest: DelegationDigest,
        delegation: Delegation,
        annotation: A,
    ) -> bool {
        if self.entries.len() >= room.min(self.capacity) {
            return false;
        }
        self.entries.push_back(TableEntry {
            digest,
            delegation,
            annotation,
        });
        true
    }

    /// Forget every entry `stale` judges stale.
    fn evict(&mut self, stale: impl Fn(&TableEntry<A>) -> bool) {
        self.entries.retain(|entry| !stale(entry));
    }

    /// Forget everything.
    fn clear(&mut self) {
        self.entries.clear();
    }

    /// The delegations currently held.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// What the sender knows about one delegation it sent inline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Acknowledgement {
    /// Sent inline; the peer has not leased it, so it still travels inline.
    Pending,
    /// The peer leased it until `until_ms` on the peer's clock: it travels by reference while
    /// `t_S + Δ < until_ms`.
    Leased { until_ms: u128 },
}

impl Acknowledgement {
    /// Whether a frame encoded at `now_ms` may reference the delegation.
    fn references_at(self, now_ms: u128) -> bool {
        matches!(self, Self::Leased { until_ms } if now_ms + LEASE_MARGIN_MS < until_ms)
    }

    /// This acknowledgement after the peer granted a lease until `until_ms`: the larger lease.
    fn leased_until(self, until_ms: u128) -> Self {
        match self {
            Self::Leased { until_ms: held } if held >= until_ms => self,
            Self::Pending | Self::Leased { .. } => Self::Leased { until_ms },
        }
    }
}

/// The sending end of one link: the delegations it has sent inline, and the lease the peer
/// granted each.
///
/// ```text
///   encode      : S × Generation × Payload × Time → S × PerSlot<DelegationRef>
///   acknowledge : S × Generation × DelegationDigest × Until → S
///   unknown     : S × Generation × DelegationDigest → S
/// ```
#[derive(Debug)]
pub(crate) struct AnnouncedDelegations {
    /// The connection generation the table belongs to; `None` until the first frame.
    generation: Option<u64>,
    announced: DelegationTable<Acknowledgement>,
}

impl AnnouncedDelegations {
    /// The sender state of a link that has carried nothing.
    pub(crate) const fn new() -> Self {
        Self {
            generation: None,
            announced: DelegationTable::new(ANNOUNCED_TABLE_CAPACITY),
        }
    }

    /// Whether the table is `generation`'s: what another generation announced was announced
    /// on another link.
    fn is_current(&self, generation: u64) -> bool {
        self.generation == Some(generation)
    }

    /// Make the table the one of `generation`: a newer generation is a new link with an empty
    /// table, an older one is a link that no longer exists. Post: `true` iff `generation` is
    /// the current one afterwards.
    fn enter(&mut self, generation: u64) -> bool {
        if self.generation.is_none_or(|current| generation > current) {
            self.generation = Some(generation);
            self.announced.clear();
        }
        self.is_current(generation)
    }

    /// Decide how each slot of `payload` travels on `generation` at `now_ms`, and remember
    /// what was sent inline.
    ///
    /// A slot whose delegation the peer leased, with more than `Δ` of the lease left, travels by
    /// digest; every other slot travels inline, and the delegation enters the table as pending
    /// if it was not held. Refused with [`Error::DelegationExpired`] when a slot's delegation
    /// expires within `Δ`: such a frame would need its delegation past its expiry in flight.
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
            .any(|delegation| delegation.is_expired_at(now_ms + LEASE_MARGIN_MS));
        if expiring {
            return Err(Error::DelegationExpired);
        }
        if !self.enter(generation) {
            return Ok(delegations.map(DelegationRef::inline));
        }
        self.announced
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
        match self.announced.touch_live(digest, now_ms) {
            Some(entry) if entry.annotation.references_at(now_ms) => {
                Ok(DelegationRef::Digest(digest))
            }
            Some(_) => Ok(DelegationRef::inline(delegation)),
            None => {
                self.announced
                    .admit(digest, || delegation.clone(), Acknowledgement::Pending);
                Ok(DelegationRef::inline(delegation))
            }
        }
    }

    /// The peer leased `digest` on `generation` until `until_ms`. A lease of a digest this end
    /// never announced, or announced on another generation, marks nothing; a stale one never
    /// shortens the lease held.
    pub(crate) fn acknowledge(
        &mut self,
        generation: u64,
        digest: DelegationDigest,
        until_ms: u128,
    ) {
        if !self.is_current(generation) {
            return;
        }
        if let Some(entry) = self.announced.get_mut(digest) {
            entry.annotation = entry.annotation.leased_until(until_ms);
        }
    }

    /// The peer does not hold `digest` on `generation`: it travels inline again until the peer
    /// leases it anew.
    pub(crate) fn unknown(&mut self, generation: u64, digest: DelegationDigest) {
        if !self.is_current(generation) {
            return;
        }
        if let Some(entry) = self.announced.get_mut(digest) {
            entry.annotation = Acknowledgement::Pending;
        }
    }
}

/// A lease this end granted on one delegation: its end, on this end's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Lease {
    until_ms: u128,
}

impl Lease {
    /// The lease granted at `now_ms` on `delegation`: [`LEASE_MS`], short of its expiry.
    fn granted(delegation: &Delegation, now_ms: u128) -> Self {
        Self {
            until_ms: delegation.expires_at_ms().min(now_ms + LEASE_MS),
        }
    }

    /// Whether a reference arriving at `now_ms` still resolves under this lease.
    fn resolves_at(self, now_ms: u128) -> bool {
        now_ms <= self.until_ms + LEASE_GRACE_MS
    }

    /// Whether a reference at `now_ms` finds less than half a lease left.
    fn wants_renewal_at(self, now_ms: u128) -> bool {
        self.until_ms.saturating_sub(now_ms) < LEASE_MS / 2
    }
}

/// A lease to tell the sender about: `Known(digest, until)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Confirmation {
    /// The delegation leased.
    pub(crate) digest: DelegationDigest,
    /// The end of the lease, on this end's clock.
    pub(crate) until_ms: u128,
}

impl Confirmation {
    /// The link-control frame that grants this lease.
    pub(crate) fn control(self) -> LinkControl {
        LinkControl::Known {
            digest: self.digest,
            until_ms: self.until_ms,
        }
    }
}

/// A frame the receiver resolved, with everything the rest of the inbound pipeline needs.
pub(crate) struct ResolvedFrame {
    /// The self-contained payload; not yet verified.
    pub(crate) payload: MessagePayload,
    /// How each slot travelled: what [`ReferencedDelegations::admit_verified`] may learn.
    pub(crate) encoding: PerSlot<SlotEncoding>,
    /// The leases the frame's references renewed, to tell the sender about.
    pub(crate) renewals: Vec<Confirmation>,
}

/// The receiving end of one link: the delegations it has carried inline, each under the lease
/// this end granted. See the module documentation for the laws.
pub(crate) struct ReferencedDelegations {
    leased: DelegationTable<Lease>,
}

impl Default for ReferencedDelegations {
    fn default() -> Self {
        Self::new()
    }
}

impl ReferencedDelegations {
    /// The receiver state of a link that has carried nothing.
    pub(crate) const fn new() -> Self {
        Self {
            leased: DelegationTable::new(REFERENCED_TABLE_CAPACITY),
        }
    }

    /// Forget every lease that ended, grace included, at `now_ms`.
    fn evict_ended(&mut self, now_ms: u128) {
        self.leased
            .evict(|entry| !entry.annotation.resolves_at(now_ms));
    }

    /// Resolve `frame` against the leases at `now_ms`, renewing a lease that has less than
    /// half its length left. `Err` names every digest the frame misses: the frame is dropped,
    /// uncharged, and the sender told about each (the repair law); nothing is held.
    pub(crate) fn resolve(
        &mut self,
        frame: Box<WirePayload<'static>>,
        now_ms: u128,
    ) -> std::result::Result<ResolvedFrame, Digests> {
        self.evict_ended(now_ms);
        let referenced: Digests = frame
            .delegation_refs()
            .into_array()
            .into_iter()
            .filter_map(|delegation| match delegation {
                DelegationRef::Inline(_) => None,
                DelegationRef::Digest(digest) => Some(*digest),
            })
            .collect();
        let missing: Digests = referenced
            .iter()
            .copied()
            .filter(|digest| self.leased.get(*digest).is_none())
            .collect();
        if !missing.is_empty() {
            return Err(missing);
        }
        let encoding = frame.delegation_refs().map(DelegationRef::encoding);
        let leased = &self.leased;
        let payload = frame
            .resolve(|delegation| match delegation {
                DelegationRef::Inline(delegation) => Ok(delegation.into_owned()),
                DelegationRef::Digest(digest) => leased
                    .get(digest)
                    .map(|entry| entry.delegation.clone())
                    .ok_or(digest),
            })
            .map_err(|digest| Digests::from([digest]))?;
        let renewals = referenced
            .into_iter()
            .filter_map(|digest| self.renew(digest, now_ms))
            .collect();
        Ok(ResolvedFrame {
            payload,
            encoding,
            renewals,
        })
    }

    /// Renew the lease on `digest` at `now_ms` if less than half of it is left and renewing
    /// lengthens it. Post: the renewed lease, to tell the sender about.
    fn renew(&mut self, digest: DelegationDigest, now_ms: u128) -> Option<Confirmation> {
        let entry = self.leased.get_mut(digest)?;
        let renewed = Lease::granted(&entry.delegation, now_ms);
        let lengthens = renewed.until_ms > entry.annotation.until_ms;
        (entry.annotation.wants_renewal_at(now_ms) && lengthens).then(|| {
            entry.annotation = renewed;
            Confirmation {
                digest,
                until_ms: renewed.until_ms,
            }
        })
    }

    /// Lease the inline delegations of a frame that verified, and name the leases to confirm
    /// to the peer: once each, every inline slot's delegation this table holds afterwards,
    /// already or newly, so a sender that keeps sending a leased delegation inline (its confirmation lost
    /// or still in flight, or its lease ended) is confirmed again, with the lease renewed. A
    /// delegation that finds no room is not leased and not confirmed: it keeps travelling
    /// inline. The origin slot leaves [`LINK_PEER_RESERVE`] entries to the hop slot.
    ///
    /// Pre: `payload` verified at `now_ms`, so each of its delegations is a live, authorized
    /// delegation; `encoding` is the [`ResolvedFrame::encoding`] it was resolved with.
    pub(crate) fn admit_verified(
        &mut self,
        payload: &MessagePayload,
        encoding: PerSlot<SlotEncoding>,
        now_ms: u128,
    ) -> Result<Vec<Confirmation>> {
        self.evict_ended(now_ms);
        let room = PerSlot {
            origin: REFERENCED_TABLE_CAPACITY - LINK_PEER_RESERVE,
            hop: REFERENCED_TABLE_CAPACITY,
        };
        let mut confirm = Vec::new();
        let slots = payload.delegations().zip(encoding).zip(room);
        for ((delegation, encoding), room) in slots.into_array() {
            if encoding != SlotEncoding::Inline {
                continue;
            }
            let digest = delegation.digest()?;
            // A peer's own frame carries one delegation in both slots: confirm it once.
            if confirm
                .iter()
                .any(|confirmation: &Confirmation| confirmation.digest == digest)
            {
                continue;
            }
            let granted = Lease::granted(delegation, now_ms);
            let leased = match self.leased.get_mut(digest) {
                Some(entry) => {
                    entry.annotation = Lease {
                        until_ms: entry.annotation.until_ms.max(granted.until_ms),
                    };
                    Some(entry.annotation)
                }
                None => self
                    .leased
                    .insert_within(room, digest, delegation.clone(), granted)
                    .then_some(granted),
            };
            if let Some(lease) = leased {
                confirm.push(Confirmation {
                    digest,
                    until_ms: lease.until_ms,
                });
            }
        }
        Ok(confirm)
    }

    /// The delegations currently leased.
    #[cfg(test)]
    pub(crate) fn known_len(&self) -> usize {
        self.leased.len()
    }
}

#[cfg(test)]
mod test_session_link;
