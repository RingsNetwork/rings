//! Session references on one link: what each end remembers, as pure state machines.
//!
//! A link is one admitted connection generation between two nodes. Every frame on it carries
//! two session slots (the origin's and the current hop's, see
//! [`WirePayload`](crate::message::WirePayload)); both ends verify both proofs, so both slots
//! matter at every hop, not only at the final destination. The few delegations behind those
//! slots repeat for the life of the link, so each direction of a link keeps one table:
//!
//! ```text
//!   sender   S : AnnouncedSessions    "sessions I sent inline, and which of them the peer confirmed"
//!   receiver R : ReferencedSessions   "sessions this link carried inline, as I verified them"
//! ```
//!
//! The link is a datagram link for all this module assumes: an accepted frame may arrive late,
//! out of order, or never. Nothing here relies on ordering; ordering only makes the tables agree
//! sooner.
//!
//! ```text
//!   sender: session s in slot ─┬─ acknowledged(s) ──▶ Digest(s)
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
//! receiver confirms only sessions of frames that verified. So on a lossless link, however
//! frames are reordered, a reference never misses: the confirmation left the receiver after the
//! session was learned, and the reference was sent after the confirmation arrived. Until the
//! confirmation arrives the sender stays inline, and the receiver confirms every inline
//! arrival, so a lost confirmation costs inline frames, never a stall.
//!
//! Law (superset): `R` keeps [`REFERENCED_TABLE_CAPACITY`] sessions and `S` only
//! [`ANNOUNCED_TABLE_CAPACITY`], half as many, under the same least-recently-referenced order
//! over the same frames. `S` therefore stops referencing a session (and sends it inline again)
//! before `R` could have forgotten it, so a miss needs something outside the tables: the two
//! ends disagreeing on expiry, or a peer that does not follow the protocol. The miss path is
//! that safety net, and it is repaired on the link: the held frame asks, the sender answers
//! from `S` (which still holds whatever it referenced recently) or disclaims.
//!
//! Law (admission): `R` learns only from frames that verified, or from an announcement some
//! held frame awaits whose delegation verifies; `S` marks only digests it announced. Nothing an
//! unrelated party says reaches either table. Law (bound): the hold keeps at most its capacity
//! in frames, each for at most the hold timeout, judged by a periodic sweep; a frame a peer
//! never backs therefore occupies this end for a bounded time whatever lifetime its proof
//! claims. Law (questions): every held frame asks for its own missing digests once, on arrival
//! (one lost question is repaired by the next frame that misses the same digest), and a frame
//! that finds the hold full asks the oldest held frame's question again; answers are one frame
//! per question. Law (order): held frames leave in arrival order relative to each other, and
//! never block a frame that resolves, because the link promises no order. Law (expiry): an
//! expired session is absent from both tables, so a reference to it is a miss and its
//! re-announcement is judged like any other: by [`Session::verify_self_at`], which refuses it.
//! Expiry forces a fresh delegation and never resurrects an old one.
//!
//! Time is an argument of every step, never read here.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::collections::VecDeque;

use crate::error::Error;
use crate::error::Result;
use crate::message::LinkControl;
use crate::message::MessagePayload;
use crate::message::PerSlot;
use crate::message::SessionRef;
use crate::message::SlotEncoding;
use crate::message::WirePayload;
use crate::session::Session;
use crate::session::SessionDigest;

/// Sessions the sending end of one link remembers: its own, and the origins of the traffic it
/// forwards, least recently referenced first out, so the working set of a busy link stays
/// resident.
pub(crate) const ANNOUNCED_TABLE_CAPACITY: usize = 64;
/// Sessions the receiving end remembers: twice the sender's, so that the sender forgets first.
/// See the superset law in the module documentation.
pub(crate) const REFERENCED_TABLE_CAPACITY: usize = 2 * ANNOUNCED_TABLE_CAPACITY;

/// The digests of a set of sessions: what a frame asks for, awaits, or confirms.
pub(crate) type Digests = BTreeSet<SessionDigest>;

/// A bounded map `SessionDigest ⇀ Session × A`, ordered from least to most recently
/// referenced, where `A` is what one end of the link annotates a session with.
///
/// Invariant: `entries.len() <= capacity`, digests are pairwise distinct, and every entry
/// satisfies `entry.digest = entry.session.digest()`.
#[derive(Debug)]
struct SessionTable<A> {
    entries: VecDeque<TableEntry<A>>,
    capacity: usize,
}

/// One session the table holds.
#[derive(Debug)]
struct TableEntry<A> {
    digest: SessionDigest,
    session: Session,
    annotation: A,
}

impl<A> SessionTable<A> {
    /// The empty table that keeps at most `capacity` sessions.
    const fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity,
        }
    }

    /// The entry addressed by `digest`, if the table holds it and its session is live at
    /// `now_ms`.
    fn live(&self, digest: SessionDigest, now_ms: u128) -> Option<&TableEntry<A>> {
        self.entries
            .iter()
            .find(|entry| entry.digest == digest)
            .filter(|entry| !entry.session.is_expired_at(now_ms))
    }

    /// Record a reference to `digest`: it becomes the most recently referenced entry. Post:
    /// `true` iff the table holds `digest`.
    fn touch(&mut self, digest: SessionDigest) -> bool {
        let Some(position) = self.entries.iter().position(|entry| entry.digest == digest) else {
            return false;
        };
        if let Some(entry) = self.entries.remove(position) {
            self.entries.push_back(entry);
        }
        true
    }

    /// Hold the session `digest` addresses as the most recently referenced entry: a reference
    /// if the table holds it, else `session()` with `annotation`, evicting the least recently
    /// referenced entry when full. The session is materialised only on first sight.
    ///
    /// Pre: `digest = session().digest()`.
    fn admit(&mut self, digest: SessionDigest, session: impl FnOnce() -> Session, annotation: A) {
        if self.touch(digest) {
            return;
        }
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(TableEntry {
            digest,
            session: session(),
            annotation,
        });
    }

    /// Change the annotation of `digest`, if held.
    fn annotate(&mut self, digest: SessionDigest, annotation: A) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.digest == digest) {
            entry.annotation = annotation;
        }
    }

    /// Forget every session expired at `now_ms`.
    fn evict_expired(&mut self, now_ms: u128) {
        self.entries
            .retain(|entry| !entry.session.is_expired_at(now_ms));
    }

    /// Forget everything.
    fn clear(&mut self) {
        self.entries.clear();
    }

    /// The sessions currently held.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// What the sender knows about one session it sent inline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Acknowledgement {
    /// Sent inline; the peer has not confirmed it, so it still travels inline.
    Pending,
    /// The peer confirmed it: it travels by reference.
    Acknowledged,
}

/// The sending end of one link: the sessions it has sent inline, and which of them the peer
/// confirmed.
///
/// ```text
///   encode      : S × Generation × Payload × Time → S × PerSlot<SessionRef>
///   acknowledge : S × Generation × SessionDigest → S
///   answer      : S × Generation × SessionDigest × Time → LinkControl
/// ```
#[derive(Debug)]
pub(crate) struct AnnouncedSessions {
    /// The connection generation the table belongs to; `None` until the first frame.
    generation: Option<u64>,
    announced: SessionTable<Acknowledgement>,
}

impl AnnouncedSessions {
    /// The sender state of a link that has carried nothing.
    pub(crate) const fn new() -> Self {
        Self {
            generation: None,
            announced: SessionTable::new(ANNOUNCED_TABLE_CAPACITY),
        }
    }

    /// Make the table the one of `generation`: a newer generation is a new link with an empty
    /// table, an older one is a link that no longer exists. Post: `true` iff `generation` is
    /// the current one afterwards.
    fn enter(&mut self, generation: u64) -> bool {
        if self.generation.is_none_or(|current| generation > current) {
            self.generation = Some(generation);
            self.announced.clear();
        }
        self.generation == Some(generation)
    }

    /// The table as `generation` sees it: this generation's table, or nothing, because what
    /// another generation announced was announced on another link.
    fn announced_on(&self, generation: u64) -> Option<&SessionTable<Acknowledgement>> {
        (self.generation == Some(generation)).then_some(&self.announced)
    }

    /// The entry `digest` addresses, if `generation` announced it and it is live at `now_ms`.
    fn live_on(
        &self,
        generation: u64,
        digest: SessionDigest,
        now_ms: u128,
    ) -> Option<&TableEntry<Acknowledgement>> {
        self.announced_on(generation)
            .and_then(|announced| announced.live(digest, now_ms))
    }

    /// Decide how each slot of `payload` travels on `generation` at `now_ms`, and remember
    /// what was sent inline.
    ///
    /// A slot whose session the peer confirmed travels by digest; every other slot travels
    /// inline, and the session enters the table as pending if it was not held.
    pub(crate) fn encode<'a>(
        &mut self,
        generation: u64,
        payload: &'a MessagePayload,
        now_ms: u128,
    ) -> Result<PerSlot<SessionRef<'a>>> {
        if !self.enter(generation) {
            return Ok(payload.sessions().map(inline));
        }
        self.announced.evict_expired(now_ms);
        let sessions = payload.sessions();
        Ok(PerSlot {
            origin: self.encode_slot(sessions.origin, now_ms)?,
            hop: self.encode_slot(sessions.hop, now_ms)?,
        })
    }

    /// [`Self::encode`] for one slot of the current generation.
    fn encode_slot<'a>(&mut self, session: &'a Session, now_ms: u128) -> Result<SessionRef<'a>> {
        let digest = session.digest()?;
        let acknowledged = self
            .announced
            .live(digest, now_ms)
            .is_some_and(|entry| entry.annotation == Acknowledgement::Acknowledged);
        self.announced
            .admit(digest, || session.clone(), Acknowledgement::Pending);
        Ok(if acknowledged {
            SessionRef::Digest(digest)
        } else {
            inline(session)
        })
    }

    /// The peer confirmed `digest` on `generation`. A confirmation of a digest this end never
    /// announced, or announced on another generation, marks nothing.
    pub(crate) fn acknowledge(&mut self, generation: u64, digest: SessionDigest) {
        if self.generation == Some(generation) {
            self.announced
                .annotate(digest, Acknowledgement::Acknowledged);
        }
    }

    /// Answer the peer's question about `digest` on `generation` at `now_ms`: the session if
    /// this link still holds it, else that it is unknown. Total, and one answer per question,
    /// so a peer cannot make this end send more frames than it asks.
    pub(crate) fn answer(
        &self,
        generation: u64,
        digest: SessionDigest,
        now_ms: u128,
    ) -> LinkControl {
        self.live_on(generation, digest, now_ms)
            .map_or(LinkControl::Unknown(digest), |entry| {
                LinkControl::Announce(entry.session.clone())
            })
    }
}

/// `session`, inline.
fn inline(session: &Session) -> SessionRef<'_> {
    SessionRef::Inline(Cow::Borrowed(session))
}

/// A frame the receiver resolved, with everything the rest of the inbound pipeline needs.
pub(crate) struct ResolvedFrame<F> {
    /// The self-contained payload; not yet verified.
    pub(crate) payload: MessagePayload,
    /// What the caller attached to the frame on arrival.
    pub(crate) carrier: F,
    /// How each slot travelled: what [`ReferencedSessions::admit_verified`] may learn.
    pub(crate) encoding: PerSlot<SlotEncoding>,
}

/// The verdict on one arriving frame.
pub(crate) enum FrameArrival<F> {
    /// Every slot resolved.
    Resolved(Box<ResolvedFrame<F>>),
    /// The frame waits for the sessions it misses, and asks for them.
    Held {
        /// The digests to ask the peer for.
        request: Digests,
    },
    /// The hold is full; the frame is dropped, and the oldest held frame's question is asked
    /// again, since a full hold means its answer is overdue.
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
    /// The digests this frame misses in `known` at `now_ms`.
    fn missing(&self, known: &SessionTable<()>, now_ms: u128) -> Digests {
        missing_digests(self.frame.as_ref(), known, now_ms)
    }

    /// Whether the frame has waited past `hold_timeout_ms` at `now_ms`, or its proof lapsed.
    fn is_stale_at(&self, now_ms: u128, hold_timeout_ms: u128) -> bool {
        now_ms.saturating_sub(self.held_at_ms) > hold_timeout_ms
            || !self.frame.hop_proof_lifetime().is_live_at(now_ms)
    }
}

/// The receiving end of one link: the sessions it has carried inline, as the receiver verified
/// them, and the frames waiting for one of them. See the module documentation for the laws.
pub(crate) struct ReferencedSessions<F> {
    known: SessionTable<()>,
    held: VecDeque<HeldFrame<F>>,
    hold_capacity: usize,
    /// How long a frame may wait for an answer: the answer is one round trip away, so a frame
    /// that waited longer is one the peer will not back.
    hold_timeout_ms: u128,
}

/// The digests among `frame`'s slots that are not live in `known` at `now_ms`.
fn missing_digests(frame: &WirePayload<'_>, known: &SessionTable<()>, now_ms: u128) -> Digests {
    frame
        .session_refs()
        .into_array()
        .into_iter()
        .filter_map(|session| match session {
            SessionRef::Inline(_) => None,
            SessionRef::Digest(digest) => Some(*digest),
        })
        .filter(|digest| known.live(*digest, now_ms).is_none())
        .collect()
}

impl<F> ReferencedSessions<F> {
    /// The receiver state of a link that has carried nothing, holding at most `hold_capacity`
    /// unresolved frames, each for at most `hold_timeout_ms`.
    pub(crate) const fn new(hold_capacity: usize, hold_timeout_ms: u128) -> Self {
        Self {
            known: SessionTable::new(REFERENCED_TABLE_CAPACITY),
            held: VecDeque::new(),
            hold_capacity,
            hold_timeout_ms,
        }
    }

    /// Resolve `frame` against the table at `now_ms`, recording each reference.
    ///
    /// Pre: `missing_digests(frame, known, now_ms)` is empty; otherwise the first missing digest
    /// is reported as [`Error::SessionReferenceUnresolved`].
    fn resolve(
        &mut self,
        frame: Box<WirePayload<'static>>,
        carrier: F,
        now_ms: u128,
    ) -> Result<Box<ResolvedFrame<F>>> {
        let encoding = frame.session_refs().map(SessionRef::encoding);
        let known = &mut self.known;
        let payload = frame.resolve(|session| -> Result<Session> {
            match session {
                SessionRef::Inline(session) => Ok(session.into_owned()),
                SessionRef::Digest(digest) => {
                    let session = known
                        .live(digest, now_ms)
                        .map(|entry| entry.session.clone())
                        .ok_or(Error::SessionReferenceUnresolved(digest))?;
                    known.touch(digest);
                    Ok(session)
                }
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
        let missing = missing_digests(frame.as_ref(), &self.known, now_ms);
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

    /// Learn the inline sessions of a frame that verified, and name the digests to confirm to
    /// the peer: every inline slot's, so a sender that keeps sending a known session inline
    /// (its confirmation lost or still in flight) is confirmed again.
    ///
    /// Pre: `payload` verified at `now_ms`, so each of its sessions is a live, authorized
    /// delegation; `encoding` is the [`ResolvedFrame::encoding`] it was resolved with.
    pub(crate) fn admit_verified(
        &mut self,
        payload: &MessagePayload,
        encoding: PerSlot<SlotEncoding>,
        now_ms: u128,
    ) -> Result<Digests> {
        self.known.evict_expired(now_ms);
        let mut confirm = Digests::new();
        for (session, encoding) in payload.sessions().zip(encoding).into_array() {
            if encoding == SlotEncoding::Inline {
                let digest = session.digest()?;
                self.known.admit(digest, || session.clone(), ());
                confirm.insert(digest);
            }
        }
        Ok(confirm)
    }

    /// Whether some held frame misses `digest` at `now_ms`: the only announcements and
    /// disclaimers this end asked for.
    fn is_awaited(&self, digest: SessionDigest, now_ms: u128) -> bool {
        self.held
            .iter()
            .any(|held| held.missing(&self.known, now_ms).contains(&digest))
    }

    /// Remove every held frame satisfying `dropped`, given the table, keeping the order of the
    /// rest.
    fn drop_held(&mut self, dropped: impl Fn(&SessionTable<()>, &HeldFrame<F>) -> bool) -> Vec<F> {
        let known = &self.known;
        let (dropped, kept): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|held| dropped(known, held));
        self.held = kept;
        dropped.into_iter().map(|held| held.carrier).collect()
    }

    /// The peer announced `session` at `now_ms`.
    ///
    /// ```text
    ///   not awaited ───────────────────────▶ Ok([])   ignored: nothing unsolicited is cached
    ///   awaited ∧ delegation verifies ─────▶ Ok([])   admitted; the caller releases
    ///   awaited ∧ delegation refused ──────▶ Ok(F*)   frames awaiting it, to be failed
    /// ```
    pub(crate) fn announce(&mut self, session: Session, now_ms: u128) -> Result<Vec<F>> {
        let digest = session.digest()?;
        if !self.is_awaited(digest, now_ms) {
            return Ok(Vec::new());
        }
        if session.verify_self_at(now_ms).is_err() {
            return Ok(self.unknown(digest, now_ms));
        }
        self.known.evict_expired(now_ms);
        self.known.admit(digest, || session, ());
        Ok(Vec::new())
    }

    /// The peer disclaimed `digest` at `now_ms`: the frames awaiting it, to be failed. A
    /// disclaimer of a digest nothing awaits drops nothing.
    pub(crate) fn unknown(&mut self, digest: SessionDigest, now_ms: u128) -> Vec<F> {
        self.drop_held(|known, held| held.missing(known, now_ms).contains(&digest))
    }

    /// Drop every frame that has waited past the hold timeout at `now_ms`, or whose proof
    /// lapsed: the frames to be failed. This is the sweep the module's bound law names.
    pub(crate) fn sweep(&mut self, now_ms: u128) -> Vec<F> {
        let hold_timeout_ms = self.hold_timeout_ms;
        self.drop_held(|_, held| held.is_stale_at(now_ms, hold_timeout_ms))
    }

    /// The oldest held frame that resolves at `now_ms`; `None` when every held frame still
    /// waits.
    pub(crate) fn release_next(&mut self, now_ms: u128) -> Result<Option<Box<ResolvedFrame<F>>>> {
        let known = &self.known;
        let Some(position) = self
            .held
            .iter()
            .position(|held| held.missing(known, now_ms).is_empty())
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

    /// The sessions currently known.
    #[cfg(test)]
    pub(crate) fn known_len(&self) -> usize {
        self.known.len()
    }
}

#[cfg(test)]
mod test_session_link;
