//! Delegation references on one link: what each end remembers, as pure state machines.
//!
//! A link is one admitted connection generation between two nodes. Every frame on it carries
//! two delegation slots (the origin's and the current hop's, see
//! [`WirePayload`](crate::message::WirePayload)); both ends verify both proofs, so both slots
//! matter at every hop, not only at the final destination. The few delegations behind those
//! slots repeat for the life of the link, so each direction of a link keeps one table:
//!
//! ```text
//!   sender   S : AnnouncedDelegations    "delegations I sent inline, and which of them the peer confirmed"
//!   receiver R : ReferencedDelegations   "delegations this link carried inline, as I verified them"
//! ```
//!
//! The link is a datagram link for all this module assumes: an accepted frame may arrive late,
//! out of order, or never. Nothing here relies on ordering; ordering only makes the tables agree
//! sooner.
//!
//! ```text
//!   sender: delegation s in slot ─┬─ acknowledged(s) ──▶ Digest(s)
//!                              └─ otherwise ───────▶ Inline(s)      (admit s as pending)
//!
//!   receiver: frame arrives ─┬─ every Digest live in R ─▶ Resolved ──verify──▶ admit inline
//!                            │                                          └──▶ Known(d) per inline slot
//!                            ├─ hold below capacity ──▶ Held{request}   (ask for what it misses)
//!                            └─ otherwise ───────────▶ Overflow{request} (dropped; the oldest
//!                                                                          held frame's question again)
//!
//!   sender:   Known(d) ─▶ acknowledge d ─▶ later frames reference d
//!   receiver: Announce(s) ─▶ admitted iff awaited ∧ delegation verifies ─▶ release what resolves
//!             Unknown(d)  ─▶ frames awaiting d dropped
//!             sweep(now)  ─▶ frames held past the hold timeout, or whose proof lapsed, dropped
//! ```
//!
//! Law (soundness): the sender references `d` only after the receiver confirmed `d`, and the
//! receiver confirms only delegations of frames that verified. So on a lossless link, however
//! frames are reordered, a reference never misses: the confirmation left the receiver after the
//! delegation was learned, and the reference was sent after the confirmation arrived. Until the
//! confirmation arrives the sender stays inline, and the receiver confirms every inline
//! arrival, so a lost confirmation costs inline frames, never a stall.
//!
//! Law (superset): `R` keeps [`REFERENCED_TABLE_CAPACITY`] delegations and `S` only
//! [`ANNOUNCED_TABLE_CAPACITY`], half as many, under the same least-recently-referenced order
//! over the frames both ends saw: `S` touches a delegation on every frame it encodes, `R` on every
//! frame it resolved or verified. On a lossless link `S` therefore stops referencing a delegation
//! (and sends it inline again) before `R` could have forgotten it. A frame `R` never sees
//! resolved (lost on the link, refused at the transport, or dropped by `R` before it verified)
//! touches `S` and not `R`, so the two orders drift and `R` may evict a delegation `S` still
//! references; that miss is answered from `S`, which still holds it, and costs one round trip
//! and no charge. `S` cannot answer only if it evicted the delegation too, between the reference
//! and the question (a full sender table of newer delegations within one round trip), or if the
//! two ends disagree on expiry, or the peer does not follow the protocol. The miss path is the
//! safety net for all of these, and it is repaired on the link: the held frame asks, the sender
//! answers from `S` or disclaims.
//!
//! Law (admission): `R` learns only from frames that verified, or from an announcement some
//! held frame awaits whose delegation verifies; `S` marks only digests it announced. Nothing an
//! unrelated party says reaches either table. Law (bound): the hold keeps at most its capacity
//! in frames, each for at most the hold timeout, judged by a periodic sweep; a frame a peer
//! never backs therefore occupies this end for a bounded time whatever lifetime its proof
//! claims. Law (questions): every held frame asks for its own missing digests once, on arrival
//! (one lost question is repaired by the next frame that misses the same digest), and a frame
//! that finds the hold full asks the oldest held frame's question again (a duplicate the
//! sender may answer twice; it is the frame's only chance to be asked about); answers are one
//! frame per question. A frame dropped for want of room is a loss at this end's capacity, not
//! the peer's fault, and is not charged; a held frame the peer does not back is, and only
//! once this end has asked: a held frame whose question was never sent (the shell reports
//! what it sent through [`ReferencedDelegations::note_asked`]) is dropped uncharged by the sweep,
//! since the peer never had its round trip. Law (order): a
//! held frame never waits for a frame held before it, and never
//! blocks a frame that resolves; among the frames resolvable at one instant, the earliest
//! arrival leaves first. The link promises no order, so nothing downstream may rely on more
//! than this. Law (expiry): an
//! expired delegation is absent from both tables, so a reference to it is a miss and its
//! re-announcement is judged like any other: by [`Delegation::verify_delegator_authorization_at`], which refuses it.
//! Expiry forces a fresh delegation and never resurrects an old one.
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

/// Sessions the sending end of one link remembers: its own, and the origins of the traffic it
/// forwards, least recently referenced first out, so the working set of a busy link stays
/// resident.
pub(crate) const ANNOUNCED_TABLE_CAPACITY: usize = 64;
/// Sessions the receiving end remembers: twice the sender's, so that the sender forgets first.
/// See the superset law in the module documentation.
pub(crate) const REFERENCED_TABLE_CAPACITY: usize = 2 * ANNOUNCED_TABLE_CAPACITY;

/// The digests of a set of delegations: what a frame asks for, awaits, or confirms.
pub(crate) type Digests = BTreeSet<DelegationDigest>;

/// The receiver's table: what it knows carries no annotation.
type KnownDelegations = DelegationTable<()>;

/// A bounded map `DelegationDigest ⇀ Delegation × A`, ordered from least to most recently
/// referenced, where `A` is what one end of the link annotates a delegation with.
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

    /// The entry addressed by `digest`, if the table holds it and its delegation is live at
    /// `now_ms`.
    fn live(&self, digest: DelegationDigest, now_ms: u128) -> Option<&TableEntry<A>> {
        self.entries
            .iter()
            .find(|entry| entry.digest == digest)
            .filter(|entry| !entry.delegation.is_expired_at(now_ms))
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

    /// [`Self::live`] and [`Self::touch`] in one pass: the entry `digest` addresses, now the
    /// most recently referenced, if the table holds it live at `now_ms`; an expired entry is
    /// neither returned nor touched.
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

    /// Change the annotation of `digest`, if held.
    fn annotate(&mut self, digest: DelegationDigest, annotation: A) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.digest == digest) {
            entry.annotation = annotation;
        }
    }

    /// Forget every delegation expired at `now_ms`.
    fn evict_expired(&mut self, now_ms: u128) {
        self.entries
            .retain(|entry| !entry.delegation.is_expired_at(now_ms));
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
    /// Sent inline; the peer has not confirmed it, so it still travels inline.
    Pending,
    /// The peer confirmed it: it travels by reference.
    Acknowledged,
}

/// The sending end of one link: the delegations it has sent inline, and which of them the peer
/// confirmed.
///
/// ```text
///   encode      : S × Generation × Payload × Time → S × PerSlot<DelegationRef>
///   acknowledge : S × Generation × DelegationDigest → S
///   answer      : S × Generation × DelegationDigest × Time → LinkControl
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
    /// A slot whose delegation the peer confirmed travels by digest; every other slot travels
    /// inline, and the delegation enters the table as pending if it was not held.
    pub(crate) fn encode<'a>(
        &mut self,
        generation: u64,
        payload: &'a MessagePayload,
        now_ms: u128,
    ) -> Result<PerSlot<DelegationRef<'a>>> {
        if !self.enter(generation) {
            return Ok(payload.delegations().map(DelegationRef::inline));
        }
        self.announced.evict_expired(now_ms);
        let delegations = payload.delegations();
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
            Some(entry) if entry.annotation == Acknowledgement::Acknowledged => {
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

    /// The peer confirmed `digest` on `generation`. A confirmation of a digest this end never
    /// announced, or announced on another generation, marks nothing.
    pub(crate) fn acknowledge(&mut self, generation: u64, digest: DelegationDigest) {
        if self.is_current(generation) {
            self.announced
                .annotate(digest, Acknowledgement::Acknowledged);
        }
    }

    /// Answer the peer's question about `digest` on `generation` at `now_ms`: the delegation if
    /// this link still holds it, else that it is unknown. Total, and one answer per question,
    /// so a peer cannot make this end send more frames than it asks.
    pub(crate) fn answer(
        &self,
        generation: u64,
        digest: DelegationDigest,
        now_ms: u128,
    ) -> LinkControl {
        if !self.is_current(generation) {
            return LinkControl::Unknown(digest);
        }
        self.announced
            .live(digest, now_ms)
            .map_or(LinkControl::Unknown(digest), |entry| {
                LinkControl::Announce(entry.delegation.clone())
            })
    }
}

/// A frame the receiver resolved, with everything the rest of the inbound pipeline needs.
pub(crate) struct ResolvedFrame<F> {
    /// The self-contained payload; not yet verified.
    pub(crate) payload: MessagePayload,
    /// What the caller attached to the frame on arrival.
    pub(crate) carrier: F,
    /// How each slot travelled: what [`ReferencedDelegations::admit_verified`] may learn.
    pub(crate) encoding: PerSlot<SlotEncoding>,
}

/// The verdict on one arriving frame.
pub(crate) enum FrameArrival<F> {
    /// Every slot resolved.
    Resolved(Box<ResolvedFrame<F>>),
    /// The frame waits for the delegations it misses, and asks for them.
    Held {
        /// The digests to ask the peer for.
        request: Digests,
    },
    /// The hold is full; the frame is dropped, and the oldest held frame's question is asked
    /// again: a duplicate at worst, and the only chance the dropped frame's arrival gives the
    /// hold to be heard.
    Overflow {
        /// The frame that found no room, for the caller to account for.
        carrier: F,
        /// The digests to ask the peer for.
        request: Digests,
    },
}

/// A frame waiting in the hold.
struct HeldFrame<F> {
    frame: Box<WirePayload<'static>>,
    carrier: F,
    /// When the frame entered the hold, on the clock every step is judged by.
    held_at_ms: u128,
}

impl<F> HeldFrame<F> {
    /// The digests this frame misses in `known` at `now_ms`: its question.
    fn missing(&self, known: &KnownDelegations, now_ms: u128) -> Digests {
        missing_references(self.frame.as_ref(), known, now_ms).collect()
    }

    /// Whether `digest` is one of the delegations this frame misses in `known` at `now_ms`.
    fn awaits(&self, digest: DelegationDigest, known: &KnownDelegations, now_ms: u128) -> bool {
        missing_references(self.frame.as_ref(), known, now_ms).any(|missing| missing == digest)
    }

    /// Whether every reference of this frame resolves in `known` at `now_ms`.
    fn is_resolvable(&self, known: &KnownDelegations, now_ms: u128) -> bool {
        missing_references(self.frame.as_ref(), known, now_ms)
            .next()
            .is_none()
    }

    /// Whether the frame has waited past `hold_timeout_ms` at `now_ms`.
    fn waited_past(&self, hold_timeout_ms: u128, now_ms: u128) -> bool {
        now_ms.saturating_sub(self.held_at_ms) > hold_timeout_ms
    }

    /// Whether the frame's hop proof, the peer's own, lapsed at `now_ms`. The hop proof is the
    /// only one judged here: it is the peer's promise about this frame, and a lapsed one would
    /// fail verification on release whatever the origin's says. The origin's proof is judged
    /// on release, as inline.
    fn proof_lapsed_at(&self, now_ms: u128) -> bool {
        !self.frame.hop_proof_lifetime().is_live_at(now_ms)
    }
}

/// The receiving end of one link: the delegations it has carried inline, as the receiver verified
/// them, and the frames waiting for one of them. See the module documentation for the laws.
pub(crate) struct ReferencedDelegations<F> {
    known: KnownDelegations,
    held: VecDeque<HeldFrame<F>>,
    hold_capacity: usize,
    /// How long a frame may wait for an answer: the answer is one round trip away, so a frame
    /// that waited longer is one the peer will not back.
    hold_timeout_ms: u128,
    /// The digests this end has asked the peer for and not yet learned or been refused: what
    /// the sweep may charge a held frame for waiting on.
    asked: Digests,
}

/// The verdict on one announcement.
pub(crate) enum Announcement<F> {
    /// Nothing held awaits the delegation: ignored, nothing unsolicited is cached.
    Ignored,
    /// Admitted; the caller releases what resolves now.
    Admitted,
    /// The delegation does not verify: the frames that awaited it, to be failed.
    Refused(Vec<F>),
}

/// What one sweep dropped.
pub(crate) struct Swept<F> {
    /// Frames the peer was asked about and did not back in time: to be charged.
    pub(crate) unanswered: Vec<F>,
    /// Frames whose question this end never managed to send: dropped uncharged, since the
    /// peer never had its round trip.
    pub(crate) unasked: Vec<F>,
}

/// The referenced delegations of `frame` that `known` does not hold live at `now_ms`, origin slot
/// first.
fn missing_references<'a>(
    frame: &'a WirePayload<'_>,
    known: &'a KnownDelegations,
    now_ms: u128,
) -> impl Iterator<Item = DelegationDigest> + 'a {
    frame
        .delegation_refs()
        .into_array()
        .into_iter()
        .filter_map(|delegation| match delegation {
            DelegationRef::Inline(_) => None,
            DelegationRef::Digest(digest) => Some(*digest),
        })
        .filter(move |digest| known.live(*digest, now_ms).is_none())
}

/// Remove every frame of `held` satisfying `dropped`, keeping the order of the rest, and
/// return the carriers of the removed frames in their arrival order.
fn drop_held<F>(
    held: &mut VecDeque<HeldFrame<F>>,
    dropped: impl Fn(&HeldFrame<F>) -> bool,
) -> Vec<F> {
    let (dropped, kept): (VecDeque<_>, VecDeque<_>) =
        std::mem::take(held).into_iter().partition(dropped);
    *held = kept;
    dropped.into_iter().map(|held| held.carrier).collect()
}

impl<F> ReferencedDelegations<F> {
    /// The receiver state of a link that has carried nothing, holding at most `hold_capacity`
    /// unresolved frames, each for at most `hold_timeout_ms`.
    pub(crate) const fn new(hold_capacity: usize, hold_timeout_ms: u128) -> Self {
        Self {
            known: DelegationTable::new(REFERENCED_TABLE_CAPACITY),
            held: VecDeque::new(),
            hold_capacity,
            hold_timeout_ms,
            asked: Digests::new(),
        }
    }

    /// Resolve `frame` against the table at `now_ms`, recording each reference.
    ///
    /// Pre: `missing_references(frame, known, now_ms)` is empty; otherwise the first missing digest
    /// is reported as [`Error::DelegationReferenceUnresolved`].
    fn resolve(
        &mut self,
        frame: Box<WirePayload<'static>>,
        carrier: F,
        now_ms: u128,
    ) -> Result<Box<ResolvedFrame<F>>> {
        let encoding = frame.delegation_refs().map(DelegationRef::encoding);
        let known = &mut self.known;
        let payload = frame.resolve(|delegation| -> Result<Delegation> {
            match delegation {
                DelegationRef::Inline(delegation) => Ok(delegation.into_owned()),
                DelegationRef::Digest(digest) => known
                    .touch_live(digest, now_ms)
                    .map(|entry| entry.delegation.clone())
                    .ok_or(Error::DelegationReferenceUnresolved(digest)),
            }
        })?;
        Ok(Box::new(ResolvedFrame {
            payload,
            carrier,
            encoding,
        }))
    }

    /// Judge one arriving frame at `now_ms`.
    pub(crate) fn arrive(
        &mut self,
        frame: Box<WirePayload<'static>>,
        carrier: F,
        now_ms: u128,
    ) -> Result<FrameArrival<F>> {
        let missing: Digests = missing_references(frame.as_ref(), &self.known, now_ms).collect();
        if missing.is_empty() {
            return self
                .resolve(frame, carrier, now_ms)
                .map(FrameArrival::Resolved);
        }
        if self.held.len() >= self.hold_capacity {
            let request = self
                .held
                .front()
                .map(|oldest| oldest.missing(&self.known, now_ms))
                .unwrap_or_default();
            return Ok(FrameArrival::Overflow { carrier, request });
        }
        self.held.push_back(HeldFrame {
            frame,
            carrier,
            held_at_ms: now_ms,
        });
        Ok(FrameArrival::Held { request: missing })
    }

    /// Learn the inline delegations of a frame that verified, and name the digests to confirm to
    /// the peer: every inline slot's, so a sender that keeps sending a known delegation inline
    /// (its confirmation lost or still in flight) is confirmed again.
    ///
    /// Pre: `payload` verified at `now_ms`, so each of its delegations is a live, authorized
    /// delegation; `encoding` is the [`ResolvedFrame::encoding`] it was resolved with.
    pub(crate) fn admit_verified(
        &mut self,
        payload: &MessagePayload,
        encoding: PerSlot<SlotEncoding>,
        now_ms: u128,
    ) -> Result<Digests> {
        self.known.evict_expired(now_ms);
        let mut confirm = Digests::new();
        for (delegation, encoding) in payload.delegations().zip(encoding).into_array() {
            if encoding == SlotEncoding::Inline {
                let digest = delegation.digest()?;
                self.known.admit(digest, || delegation.clone(), ());
                self.asked.remove(&digest);
                confirm.insert(digest);
            }
        }
        Ok(confirm)
    }

    /// Record that the peer was asked for `digests`: a held frame awaiting one of them may
    /// now be charged for waiting past the hold timeout.
    pub(crate) fn note_asked(&mut self, digests: impl IntoIterator<Item = DelegationDigest>) {
        self.asked.extend(digests);
    }

    /// Whether some held frame misses `digest` at `now_ms`: the only announcements and
    /// disclaimers this end asked for.
    fn is_awaited(&self, digest: DelegationDigest, now_ms: u128) -> bool {
        self.held
            .iter()
            .any(|held| held.awaits(digest, &self.known, now_ms))
    }

    /// Whether some held frame misses one of `digests` at `now_ms`: whether learning them
    /// releases anything.
    pub(crate) fn awaits_any(&self, digests: &Digests, now_ms: u128) -> bool {
        digests
            .iter()
            .any(|digest| self.is_awaited(*digest, now_ms))
    }

    /// The peer announced `delegation` at `now_ms`.
    ///
    /// ```text
    ///   not awaited ───────────────────────▶ Ignored      nothing unsolicited is cached
    ///   awaited ∧ delegation verifies ─────▶ Admitted     the caller releases
    ///   awaited ∧ delegation refused ──────▶ Refused(F*)  frames awaiting it, to be failed
    /// ```
    pub(crate) fn announce(
        &mut self,
        delegation: Delegation,
        now_ms: u128,
    ) -> Result<Announcement<F>> {
        let digest = delegation.digest()?;
        if !self.is_awaited(digest, now_ms) {
            return Ok(Announcement::Ignored);
        }
        if delegation
            .verify_delegator_authorization_at(now_ms)
            .is_err()
        {
            return Ok(Announcement::Refused(self.unknown(digest, now_ms)));
        }
        self.known.evict_expired(now_ms);
        self.known.admit(digest, || delegation, ());
        self.asked.remove(&digest);
        Ok(Announcement::Admitted)
    }

    /// The peer disclaimed `digest` at `now_ms`: the frames awaiting it, to be failed. A
    /// disclaimer of a digest nothing awaits drops nothing.
    pub(crate) fn unknown(&mut self, digest: DelegationDigest, now_ms: u128) -> Vec<F> {
        self.asked.remove(&digest);
        let known = &self.known;
        drop_held(&mut self.held, |held| held.awaits(digest, known, now_ms))
    }

    /// Drop every frame that has waited past the hold timeout at `now_ms`, or whose proof
    /// lapsed, telling apart the frames this end asked about from those it never managed to
    /// ask about. This is the sweep the module's bound law names.
    pub(crate) fn sweep(&mut self, now_ms: u128) -> Swept<F> {
        let hold_timeout_ms = self.hold_timeout_ms;
        let known = &self.known;
        let asked = &self.asked;
        let mut swept = Swept {
            unanswered: Vec::new(),
            unasked: Vec::new(),
        };
        let (stale, kept): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|held| {
                held.waited_past(hold_timeout_ms, now_ms) || held.proof_lapsed_at(now_ms)
            });
        self.held = kept;
        for held in stale {
            let was_asked = missing_references(held.frame.as_ref(), known, now_ms)
                .all(|missing| asked.contains(&missing));
            if was_asked {
                swept.unanswered.push(held.carrier);
            } else {
                swept.unasked.push(held.carrier);
            }
        }
        swept
    }

    /// The earliest-arrived held frame that resolves at `now_ms`; `None` when every held frame
    /// still waits. See the order law.
    pub(crate) fn release_next(&mut self, now_ms: u128) -> Result<Option<Box<ResolvedFrame<F>>>> {
        let known = &self.known;
        let Some(position) = self
            .held
            .iter()
            .position(|held| held.is_resolvable(known, now_ms))
        else {
            return Ok(None);
        };
        let Some(HeldFrame { frame, carrier, .. }) = self.held.remove(position) else {
            return Ok(None);
        };
        self.resolve(frame, carrier, now_ms).map(Some)
    }

    /// The frames currently held.
    #[cfg(test)]
    pub(crate) fn held_len(&self) -> usize {
        self.held.len()
    }

    /// The delegations currently known.
    #[cfg(test)]
    pub(crate) fn known_len(&self) -> usize {
        self.known.len()
    }
}

#[cfg(test)]
mod test_session_link;
