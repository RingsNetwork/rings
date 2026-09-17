//! Session references on one link: what each end remembers, as pure state machines.
//!
//! A link is one admitted connection generation between two nodes. Every frame on it carries
//! two session slots (the origin's and the current hop's, see
//! [`WirePayload`](crate::message::WirePayload)); both ends verify both proofs, so both slots
//! matter at every hop, not only at the final destination. The few delegations behind those
//! slots repeat for the life of the link, so each direction of a link keeps one table:
//!
//! ```text
//!   sender   S : AnnouncedSessions    "sessions this link has carried inline, as I sent them"
//!   receiver R : ReferencedSessions   "sessions this link has carried inline, as I verified them"
//! ```
//!
//! The sender replaces a slot by its digest iff the digest is live in `S`; the receiver resolves
//! a digest from `R`. The scope is the link on purpose:
//!
//! - *Who may populate `R`*: only the peer at the other end, only with sessions of frames that
//!   verified, or with a solicited announcement whose delegation verified. A peer can therefore
//!   spend only its own table, which is bounded, and nothing an unrelated party says is cached.
//! - *Relay hops*: a forwarding hop re-encodes the origin slot for its own next link, from its
//!   own `S`; the origin's signature does not cover the slot. No hop ever asks the origin for
//!   anything, so a miss costs one round trip on one link, never a routed lookup.
//! - *Restart*: both tables die with the connection generation, so a restarted end and its peer
//!   start from empty tables together.
//!
//! The tables agree when frames arrive in the order they were accepted for sending. Agreement
//! is an optimisation, not an assumption: a digest `R` cannot resolve is a *miss*, repaired on
//! the link itself.
//!
//! ```text
//!   arrive(frame)
//!     │ hold empty ∧ every digest live in R ───────────────▶ Resolved ──verify──▶ admit inline
//!     │ otherwise, hold below capacity ─▶ Held ─▶ drain
//!     │ otherwise ─▶ Overflow (frame dropped; the caller drains, which asks again)
//!
//!   drain: release_next
//!     │ head lapsed ─────────────────────────▶ Lapsed (frame dropped), continue
//!     │ head resolvable ─────────────────────▶ Resolved, continue
//!     │ head misses D ─▶ Blocked{D} ──Request(d) for d ∈ D ──▶ peer
//!     │ empty ───────────────────────────────▶ Drained
//!
//!   peer answers from S:  Announce(s) ─▶ announce ─▶ drain
//!                         Unknown(d)  ─▶ unknown  ─▶ frames missing d dropped ─▶ drain
//! ```
//!
//! Law (order): frames leave the receiver in arrival order; a frame that arrives while the hold
//! is occupied or draining queues behind it. Law (bound): `R` and `S` hold at most
//! [`SESSION_TABLE_CAPACITY`] sessions and the hold at most its capacity in frames. Law
//! (questions): only the head's missing digests are ever requested, once per drain attempt, and
//! drain attempts are caused only by arrivals and answers; there is no timer, and a peer that
//! never answers stalls only its own link. Law (expiry): an expired session is absent from both
//! tables, so a reference to it is a miss and its re-announcement is judged like any other: by
//! [`Session::verify_self_at`], which refuses it. Expiry therefore forces a fresh delegation to
//! be announced and never resurrects an old one.
//!
//! Time is an argument of every step, never read here.

use std::borrow::Cow;
use std::cmp::Ordering;
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

/// A bounded map `SessionDigest ⇀ Session`, ordered from least to most recently referenced.
///
/// Invariant: `entries.len() <= capacity`, digests are pairwise distinct, and every entry
/// satisfies `entry.digest = entry.session.digest()`.
#[derive(Debug)]
struct SessionTable {
    entries: VecDeque<(SessionDigest, Session)>,
    capacity: usize,
}

impl SessionTable {
    /// The empty table that keeps at most `capacity` sessions.
    const fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity,
        }
    }

    /// The session addressed by `digest`, if the table holds it and it is live at `now_ms`.
    fn live(&self, digest: SessionDigest, now_ms: u128) -> Option<&Session> {
        self.entries
            .iter()
            .find(|(held, _)| *held == digest)
            .map(|(_, session)| session)
            .filter(|session| !session.is_expired_at(now_ms))
    }

    /// Record a reference to `digest`: it becomes the most recently referenced entry.
    fn touch(&mut self, digest: SessionDigest) {
        if let Some(position) = self.entries.iter().position(|(held, _)| *held == digest) {
            if let Some(entry) = self.entries.remove(position) {
                self.entries.push_back(entry);
            }
        }
    }

    /// Hold `session` under `digest` as the most recently referenced entry, evicting the least
    /// recently referenced one when full. Idempotent on the set of entries.
    ///
    /// Pre: `digest = session.digest()`.
    fn admit(&mut self, digest: SessionDigest, session: Session) {
        self.entries.retain(|(held, _)| *held != digest);
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((digest, session));
    }

    /// Forget every session expired at `now_ms`.
    fn evict_expired(&mut self, now_ms: u128) {
        self.entries
            .retain(|(_, session)| !session.is_expired_at(now_ms));
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

/// How the sender fills one slot of one frame.
#[derive(Debug, PartialEq, Eq)]
enum SlotPlan {
    /// The link already carried this session: send its digest.
    Reference(SessionDigest),
    /// The link has not carried this session, or it lapsed: send it inline. The copy is what
    /// the table will hold once the frame is accepted.
    Announce(SessionDigest, Box<Session>),
}

/// The sender's decision for one frame: data returned by [`AnnouncedSessions::plan`] and given
/// back to [`AnnouncedSessions::commit`] once the frame is accepted for sending.
#[derive(Debug)]
pub(crate) struct FramePlan {
    generation: u64,
    slots: PerSlot<SlotPlan>,
}

impl FramePlan {
    /// The references that realise this plan over `payload`.
    ///
    /// Pre: this plan was made for `payload`.
    pub(crate) fn session_refs<'a>(&self, payload: &'a MessagePayload) -> PerSlot<SessionRef<'a>> {
        self.slots
            .as_ref()
            .zip(payload.sessions())
            .map(|(plan, session)| plan.session_ref(session))
    }
}

impl SlotPlan {
    /// The reference this plan puts in a slot holding `session`.
    fn session_ref<'a>(&self, session: &'a Session) -> SessionRef<'a> {
        match self {
            Self::Reference(digest) => SessionRef::Digest(*digest),
            Self::Announce(..) => SessionRef::Inline(Cow::Borrowed(session)),
        }
    }
}

/// The sending end of one link: the sessions it has carried inline, as the sender knows it.
///
/// `plan` is pure and `commit` applies it, so a frame that was planned but never accepted for
/// sending (cancelled, failed, superseded) announces nothing:
///
/// ```text
///   plan   : S × Generation × Payload × Time → FramePlan
///   commit : S × FramePlan × Time → S
/// ```
///
/// Law (soundness): `commit` is applied only to accepted frames, in acceptance order, so every
/// digest `plan` emits was carried inline by an earlier accepted frame of the same generation.
#[derive(Debug)]
pub(crate) struct AnnouncedSessions {
    /// The connection generation the table belongs to.
    generation: u64,
    announced: SessionTable,
}

impl AnnouncedSessions {
    /// The sender state of a link that has carried nothing.
    pub(crate) const fn new() -> Self {
        Self {
            generation: 0,
            announced: SessionTable::new(SESSION_TABLE_CAPACITY),
        }
    }

    /// The table as `generation` sees it: this generation's table, or nothing, because what
    /// another generation announced was announced on another link.
    fn announced_on(&self, generation: u64) -> Option<&SessionTable> {
        (self.generation == generation).then_some(&self.announced)
    }

    /// The session `digest` addresses, if `generation` announced it and it is live at `now_ms`.
    fn live_on(&self, generation: u64, digest: SessionDigest, now_ms: u128) -> Option<&Session> {
        self.announced_on(generation)
            .and_then(|announced| announced.live(digest, now_ms))
    }

    /// Decide how each slot of `payload` travels on `generation` at `now_ms`.
    pub(crate) fn plan(
        &self,
        generation: u64,
        payload: &MessagePayload,
        now_ms: u128,
    ) -> Result<FramePlan> {
        let plan_slot = |session: &Session| -> Result<SlotPlan> {
            let digest = session.digest()?;
            Ok(match self.live_on(generation, digest, now_ms) {
                Some(_) => SlotPlan::Reference(digest),
                None => SlotPlan::Announce(digest, Box::new(session.clone())),
            })
        };
        let sessions = payload.sessions();
        Ok(FramePlan {
            generation,
            slots: PerSlot {
                origin: plan_slot(sessions.origin)?,
                hop: plan_slot(sessions.hop)?,
            },
        })
    }

    /// Record that the frame planned as `plan` was accepted for sending at `now_ms`.
    ///
    /// A plan of a newer generation starts that generation's table; a plan of an older one is
    /// about a link that no longer exists and records nothing.
    pub(crate) fn commit(&mut self, plan: FramePlan, now_ms: u128) {
        match plan.generation.cmp(&self.generation) {
            Ordering::Less => return,
            Ordering::Equal => {}
            Ordering::Greater => {
                self.generation = plan.generation;
                self.announced.clear();
            }
        }
        self.announced.evict_expired(now_ms);
        for slot in plan.slots.into_array() {
            match slot {
                SlotPlan::Reference(digest) => self.announced.touch(digest),
                SlotPlan::Announce(digest, session) => self.announced.admit(digest, *session),
            }
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
            .map_or(SessionControl::Unknown(digest), |session| {
                SessionControl::Announce(session.clone())
            })
    }
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
    /// Nothing is ahead of the frame and every slot resolved.
    Resolved(Box<ResolvedFrame<F>>),
    /// The frame is queued: behind a miss of its own, or behind earlier held frames.
    Held,
    /// The hold is full; the frame is handed back undelivered.
    Overflow(F),
}

/// One step of a drain.
pub(crate) enum FrameRelease<F> {
    /// The head resolved: deliver it, then continue.
    Resolved(Box<ResolvedFrame<F>>),
    /// The head's proof lifetime lapsed while it waited: it is dropped; continue.
    Lapsed(F),
    /// The head misses these digests: ask the peer for them. The drain is over.
    Blocked(Vec<SessionDigest>),
    /// Nothing is held. The drain is over.
    Drained,
}

/// A frame waiting in the hold.
struct HeldFrame<F> {
    frame: Box<WirePayload<'static>>,
    carrier: F,
}

/// The receiving end of one link: the sessions it has carried inline, as the receiver verified
/// them, and the frames waiting for one of them. See the module documentation for the laws.
pub(crate) struct ReferencedSessions<F> {
    known: SessionTable,
    held: VecDeque<HeldFrame<F>>,
    /// One drainer is releasing held frames; arrivals queue behind them.
    draining: bool,
    hold_capacity: usize,
}

/// The digests among `frame`'s slots that are not live in `known` at `now_ms`.
fn missing_digests(
    frame: &WirePayload<'_>,
    known: &SessionTable,
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
        .fold(Vec::new(), |mut missing, digest| {
            if !missing.contains(&digest) {
                missing.push(digest);
            }
            missing
        })
}

impl<F> ReferencedSessions<F> {
    /// The receiver state of a link that has carried nothing, holding at most `hold_capacity`
    /// unresolved frames.
    pub(crate) const fn new(hold_capacity: usize) -> Self {
        Self {
            known: SessionTable::new(SESSION_TABLE_CAPACITY),
            held: VecDeque::new(),
            draining: false,
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
                        .cloned()
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

    /// Judge one arriving frame at `now_ms`.
    pub(crate) fn arrive(
        &mut self,
        frame: Box<WirePayload<'static>>,
        carrier: F,
        now_ms: u128,
    ) -> Result<FrameArrival<F>> {
        let nothing_ahead = self.held.is_empty() && !self.draining;
        if nothing_ahead && missing_digests(frame.as_ref(), &self.known, now_ms).is_empty() {
            return self
                .resolve(frame, carrier, now_ms)
                .map(FrameArrival::Resolved);
        }
        if self.held.len() >= self.hold_capacity {
            return Ok(FrameArrival::Overflow(carrier));
        }
        self.held.push_back(HeldFrame { frame, carrier });
        Ok(FrameArrival::Held)
    }

    /// Learn the inline sessions of a frame that verified.
    ///
    /// Pre: `payload` verified at `now_ms`, so each of its sessions is a live, authorized
    /// delegation; `inline` is the [`ResolvedFrame::inline`] it was resolved with.
    pub(crate) fn admit_verified(
        &mut self,
        payload: &MessagePayload,
        inline: PerSlot<bool>,
        now_ms: u128,
    ) -> Result<()> {
        self.known.evict_expired(now_ms);
        for (session, arrived_inline) in payload.sessions().zip(inline).into_array() {
            if arrived_inline {
                self.known.admit(session.digest()?, session.clone());
            }
        }
        Ok(())
    }

    /// Whether some held frame misses `digest` at `now_ms`: the only announcements and
    /// disclaimers this end asked for.
    fn is_awaited(&self, digest: SessionDigest, now_ms: u128) -> bool {
        self.held
            .iter()
            .any(|held| missing_digests(held.frame.as_ref(), &self.known, now_ms).contains(&digest))
    }

    /// Drop every held frame that misses `digest` at `now_ms`.
    fn drop_awaiting(&mut self, digest: SessionDigest, now_ms: u128) -> Vec<F> {
        let (dropped, kept): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut self.held)
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
    ///   awaited ∧ delegation verifies ─────▶ Ok([])   admitted; the caller drains
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
        self.known.admit(digest, session);
        Ok(Vec::new())
    }

    /// The peer disclaimed `digest` at `now_ms`: the frames awaiting it, to be failed. A
    /// disclaimer of a digest nothing awaits drops nothing.
    pub(crate) fn unknown(&mut self, digest: SessionDigest, now_ms: u128) -> Vec<F> {
        self.drop_awaiting(digest, now_ms)
    }

    /// Claim the drain.
    ///
    /// Post: `true` implies the caller is the only drainer until [`Self::release_next`] returns
    /// [`FrameRelease::Blocked`] or [`FrameRelease::Drained`]; `false` means another drainer is active or
    /// nothing is held.
    pub(crate) fn begin_drain(&mut self) -> bool {
        if self.draining || self.held.is_empty() {
            return false;
        }
        self.draining = true;
        true
    }

    /// The next step of the drain at `now_ms`, in arrival order.
    ///
    /// Pre: the caller holds the drain granted by [`Self::begin_drain`]. An error ends the drain
    /// as [`FrameRelease::Blocked`] does, so the hold is never left claimed by nobody.
    pub(crate) fn release_next(&mut self, now_ms: u128) -> Result<FrameRelease<F>> {
        let Some(head) = self.held.front() else {
            self.draining = false;
            return Ok(FrameRelease::Drained);
        };
        let lapsed = !head.frame.hop_proof_lifetime().is_live_at(now_ms);
        let missing = missing_digests(head.frame.as_ref(), &self.known, now_ms);
        if !lapsed && !missing.is_empty() {
            self.draining = false;
            return Ok(FrameRelease::Blocked(missing));
        }
        let Some(HeldFrame { frame, carrier }) = self.held.pop_front() else {
            self.draining = false;
            return Ok(FrameRelease::Drained);
        };
        if lapsed {
            return Ok(FrameRelease::Lapsed(carrier));
        }
        let resolved = self.resolve(frame, carrier, now_ms);
        if resolved.is_err() {
            self.draining = false;
        }
        resolved.map(FrameRelease::Resolved)
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
