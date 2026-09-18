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
//!                            ├─ hold below capacity ──▶ Held{request}   (ask for what is missing)
//!                            └─ otherwise ───────────▶ Overflow{request} (frame dropped, ask again)
//!
//!   sender:   Known(d) ─▶ acknowledge d ─▶ later frames reference d
//!   receiver: Announce(s) ─▶ admitted iff awaited ∧ delegation verifies ─▶ release what resolves
//!             Unknown(d)  ─▶ frames awaiting d dropped
//! ```
//!
//! Law (soundness): the sender references `d` only after the receiver confirmed `d`, and the
//! receiver confirms only sessions of frames that verified. Hence on a lossless link, however
//! frames are reordered, a reference never misses: the confirmation left the receiver after the
//! session was learned, and the reference was sent after the confirmation arrived. A miss
//! needs the receiver to have *forgotten* (capacity eviction, expiry), and is repaired on the
//! link. Until the confirmation arrives the sender stays inline, and the receiver confirms
//! every inline arrival of a session it knows, so a lost confirmation costs inline frames, never
//! a stall.
//!
//! Law (admission): `R` learns only from frames that verified, or from an announcement some
//! held frame awaits whose delegation verifies; `S` marks only digests it announced. Nothing an
//! unrelated party says reaches either table. Law (bound): `R` and `S` hold at most
//! [`SESSION_TABLE_CAPACITY`] sessions and the hold at most its capacity in frames; a held frame
//! is released or dropped, never reordered against anything, because the link promises no
//! order. Law (questions): a question is asked for a missing digest when the first frame awaiting
//! it is held, and again when the hold overflows; answers are one frame per question. There is
//! no timer: a peer that never answers stalls only its own held frames, which lapse with their
//! proofs. Law (expiry): an expired session is absent from both tables, so a reference to it is
//! a miss and its re-announcement is judged like any other: by [`Session::verify_self_at`],
//! which refuses it. Expiry forces a fresh delegation and never resurrects an old one.
//!
//! Time is an argument of every step, never read here.

use std::borrow::Cow;
use std::collections::VecDeque;

use crate::error::Error;
use crate::error::Result;
use crate::message::MessagePayload;
use crate::message::PerSlot;
use crate::message::SessionControl;
use crate::message::SessionRef;
use crate::message::WirePayload;
use crate::session::Session;
use crate::session::SessionDigest;

/// Sessions one direction of one link remembers. A relay keeps the hop session of its peer and
/// the origin sessions of the traffic it carries; the least recently referenced one leaves
/// first, so the working set of a busy link stays resident.
pub(crate) const SESSION_TABLE_CAPACITY: usize = 64;

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

    /// Record a reference to `digest`: it becomes the most recently referenced entry.
    fn touch(&mut self, digest: SessionDigest) {
        if let Some(position) = self.entries.iter().position(|entry| entry.digest == digest) {
            if let Some(entry) = self.entries.remove(position) {
                self.entries.push_back(entry);
            }
        }
    }

    /// Hold `session` under `digest` with `annotation` as the most recently referenced entry,
    /// evicting the least recently referenced one when full. Idempotent on the set of digests.
    ///
    /// Pre: `digest = session.digest()`.
    fn admit(&mut self, digest: SessionDigest, session: Session, annotation: A) {
        self.entries.retain(|entry| entry.digest != digest);
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(TableEntry {
            digest,
            session,
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
///   answer      : S × Generation × SessionDigest × Time → SessionControl
/// ```
#[derive(Debug)]
pub(crate) struct AnnouncedSessions {
    /// The connection generation the table belongs to.
    generation: u64,
    announced: SessionTable<Acknowledgement>,
}

impl AnnouncedSessions {
    /// The sender state of a link that has carried nothing.
    pub(crate) const fn new() -> Self {
        Self {
            generation: 0,
            announced: SessionTable::new(SESSION_TABLE_CAPACITY),
        }
    }

    /// Make the table the one of `generation`: a newer generation is a new link with an empty
    /// table, an older one is a link that no longer exists. Post: `true` iff `generation` is
    /// the current one afterwards.
    fn enter(&mut self, generation: u64) -> bool {
        if generation > self.generation {
            self.generation = generation;
            self.announced.clear();
        }
        self.generation == generation
    }

    /// The table as `generation` sees it: this generation's table, or nothing, because what
    /// another generation announced was announced on another link.
    fn announced_on(&self, generation: u64) -> Option<&SessionTable<Acknowledgement>> {
        (self.generation == generation).then_some(&self.announced)
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
        match self.announced.live(digest, now_ms) {
            Some(entry) if entry.annotation == Acknowledgement::Acknowledged => {
                self.announced.touch(digest);
                Ok(SessionRef::Digest(digest))
            }
            Some(_) => {
                self.announced.touch(digest);
                Ok(inline(session))
            }
            None => {
                self.announced
                    .admit(digest, session.clone(), Acknowledgement::Pending);
                Ok(inline(session))
            }
        }
    }

    /// The peer confirmed `digest` on `generation`. A confirmation of a digest this end never
    /// announced, or announced on another generation, marks nothing.
    pub(crate) fn acknowledge(&mut self, generation: u64, digest: SessionDigest) {
        if self.generation == generation {
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
    ) -> SessionControl {
        self.live_on(generation, digest, now_ms)
            .map_or(SessionControl::Unknown(digest), |entry| {
                SessionControl::Announce(entry.session.clone())
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
    /// Which slots arrived inline: what [`ReferencedSessions::admit_verified`] may learn.
    pub(crate) inline: PerSlot<bool>,
}

/// The verdict on one arriving frame.
pub(crate) enum FrameArrival<F> {
    /// Every slot resolved.
    Resolved(Box<ResolvedFrame<F>>),
    /// The frame waits for the sessions it misses; `request` names those nothing held before
    /// it was already waiting for.
    Held {
        /// The digests to ask the peer for.
        request: Vec<SessionDigest>,
    },
    /// The hold is full; the frame is dropped, and every awaited digest is asked for again,
    /// since a full hold means an answer is overdue.
    Overflow {
        /// The digests to ask the peer for.
        request: Vec<SessionDigest>,
    },
}

/// One held frame leaving the hold.
pub(crate) enum FrameRelease<F> {
    /// The frame resolved: deliver it.
    Resolved(Box<ResolvedFrame<F>>),
    /// The frame's proof lifetime lapsed while it waited: it is dropped.
    Lapsed(F),
}

/// A frame waiting in the hold.
struct HeldFrame<F> {
    frame: Box<WirePayload<'static>>,
    carrier: F,
}

/// The receiving end of one link: the sessions it has carried inline, as the receiver verified
/// them, and the frames waiting for one of them. See the module documentation for the laws.
pub(crate) struct ReferencedSessions<F> {
    known: SessionTable<()>,
    held: Vec<HeldFrame<F>>,
    hold_capacity: usize,
}

/// The digests among `frame`'s slots that are not live in `known` at `now_ms`, each once.
fn missing_digests(
    frame: &WirePayload<'_>,
    known: &SessionTable<()>,
    now_ms: u128,
) -> Vec<SessionDigest> {
    frame
        .session_refs()
        .into_array()
        .into_iter()
        .filter_map(|session| match session {
            SessionRef::Inline(_) => None,
            SessionRef::Digest(digest) => Some(*digest),
        })
        .filter(|digest| known.live(*digest, now_ms).is_none())
        .fold(Vec::new(), push_distinct)
}

/// `digests` with `digest` appended unless already present: a set kept as an ordered vector.
fn push_distinct(mut digests: Vec<SessionDigest>, digest: SessionDigest) -> Vec<SessionDigest> {
    if !digests.contains(&digest) {
        digests.push(digest);
    }
    digests
}

impl<F> ReferencedSessions<F> {
    /// The receiver state of a link that has carried nothing, holding at most `hold_capacity`
    /// unresolved frames.
    pub(crate) const fn new(hold_capacity: usize) -> Self {
        Self {
            known: SessionTable::new(SESSION_TABLE_CAPACITY),
            held: Vec::new(),
            hold_capacity,
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
        let inline = frame
            .session_refs()
            .map(|session| matches!(session, SessionRef::Inline(_)));
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
            inline,
        }))
    }

    /// Every digest some held frame misses at `now_ms`, each once, in hold order.
    fn awaited_digests(&self, now_ms: u128) -> Vec<SessionDigest> {
        self.held
            .iter()
            .flat_map(|held| missing_digests(held.frame.as_ref(), &self.known, now_ms))
            .fold(Vec::new(), push_distinct)
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
        let awaited = self.awaited_digests(now_ms);
        if self.held.len() >= self.hold_capacity {
            return Ok(FrameArrival::Overflow {
                request: missing.into_iter().fold(awaited, push_distinct),
            });
        }
        let request = missing
            .into_iter()
            .filter(|digest| !awaited.contains(digest))
            .collect();
        self.held.push(HeldFrame { frame, carrier });
        Ok(FrameArrival::Held { request })
    }

    /// Learn the inline sessions of a frame that verified, and name the digests to confirm to
    /// the peer: every inline slot's, so a sender that keeps sending a known session inline
    /// (its confirmation lost or still in flight) is confirmed again.
    ///
    /// Pre: `payload` verified at `now_ms`, so each of its sessions is a live, authorized
    /// delegation; `inline` is the [`ResolvedFrame::inline`] it was resolved with.
    pub(crate) fn admit_verified(
        &mut self,
        payload: &MessagePayload,
        inline: PerSlot<bool>,
        now_ms: u128,
    ) -> Result<Vec<SessionDigest>> {
        self.known.evict_expired(now_ms);
        let mut confirm = Vec::new();
        for (session, arrived_inline) in payload.sessions().zip(inline).into_array() {
            if arrived_inline {
                let digest = session.digest()?;
                self.known.admit(digest, session.clone(), ());
                confirm = push_distinct(confirm, digest);
            }
        }
        Ok(confirm)
    }

    /// Whether some held frame misses `digest` at `now_ms`: the only announcements and
    /// disclaimers this end asked for.
    fn is_awaited(&self, digest: SessionDigest, now_ms: u128) -> bool {
        self.awaited_digests(now_ms).contains(&digest)
    }

    /// Drop every held frame that misses `digest` at `now_ms`.
    fn drop_awaiting(&mut self, digest: SessionDigest, now_ms: u128) -> Vec<F> {
        let (dropped, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|held| {
                missing_digests(held.frame.as_ref(), &self.known, now_ms).contains(&digest)
            });
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
            return Ok(self.drop_awaiting(digest, now_ms));
        }
        self.known.evict_expired(now_ms);
        self.known.admit(digest, session, ());
        Ok(Vec::new())
    }

    /// The peer disclaimed `digest` at `now_ms`: the frames awaiting it, to be failed. A
    /// disclaimer of a digest nothing awaits drops nothing.
    pub(crate) fn unknown(&mut self, digest: SessionDigest, now_ms: u128) -> Vec<F> {
        self.drop_awaiting(digest, now_ms)
    }

    /// The next held frame that can leave at `now_ms`: one whose proof lapsed, else one that
    /// resolves now; `None` when every held frame still waits.
    pub(crate) fn release_next(&mut self, now_ms: u128) -> Result<Option<FrameRelease<F>>> {
        let releasable = self.held.iter().position(|held| {
            !held.frame.hop_proof_lifetime().is_live_at(now_ms)
                || missing_digests(held.frame.as_ref(), &self.known, now_ms).is_empty()
        });
        let Some(position) = releasable else {
            return Ok(None);
        };
        let HeldFrame { frame, carrier } = self.held.swap_remove(position);
        if !frame.hop_proof_lifetime().is_live_at(now_ms) {
            return Ok(Some(FrameRelease::Lapsed(carrier)));
        }
        self.resolve(frame, carrier, now_ms)
            .map(|resolved| Some(FrameRelease::Resolved(resolved)))
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
