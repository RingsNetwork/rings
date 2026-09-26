//! Test builds: frame conservation of one node's transport, for the test harness's quiescence.
//!
//! Every frame a node exchanges with another node crosses two chokepoints: it leaves through
//! `SwarmConnection::send_data` and enters through the link stage's `submit_inbound_message`.
//! The ledger counts both, at the frame's first and last moment inside this crate:
//!
//! ```text
//!   send_data ─ sending ↑ ─ transport commits ─ … ─ sent ↑, sending ↓
//!                                  ┆ the wire, a transport's delay
//!   submit_inbound_message ─ in_flight ↑, arrived ↑ ─ decode ─ … ─ handoff ─ in_flight ↓
//!   on_invalid_inbound_frame ─ in_flight ↑, arrived ↑, in_flight ↓
//! ```
//!
//! Commit law: a send counts as sent once the transport has committed to it, when it returned
//! `Ok` or its acceptance became irrevocable, even if the send future is dropped afterwards. So
//! a frame that can arrive is counted as sent, and every frame the swarm can lose below itself
//! leaves `Σ sent ≥ Σ arrived`: a loss never balances another frame's deficit, it only holds the
//! wait at its hang guard.
//!
//! Conservation law: over all nodes of a test, a frame is on the wire exactly while it is counted
//! as sent and not as arrived. Hence, with nothing sending,
//! `Σ sent = Σ arrived ⇔ no frame is on the wire`, unless a frame was lost below the swarm. A
//! frame the transport rejects as malformed or oversized reaches `on_invalid_inbound_frame` and
//! arrives there, so it is not such a loss.
//!
//! Sampling law: a reader that sees a transition's later counter also sees its earlier witness.
//! Writers raise `in_flight` before `arrived`, and `sent` before lowering `sending`; readers take
//! [`FrameLedger::sample`], which loads `sending`, `sent`, `arrived`, `in_flight` in that order.
//! With release increments and acquire loads, a sample that counts an arrival sees the frame in
//! flight or already handed off, and a sample that sees a send ended sees it counted as sent.
//!
//! Coverage law: a frame that arrived is `in_flight` from before it is decoded until its lease
//! is released at the inbound actor's handoff, where the actor's capacity permit already covers
//! it. A frame dropped anywhere between (malformed, refused, superseded, held and swept) is
//! released there, so every drop balances and no frame falls between two witnesses.
//!
//! Every transition records activity, so an activity-woken wait observes each of them.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use rings_transport::core::transport::SendAcceptance;

use crate::tests::activity::record_activity;

/// Test builds: the frame counts of one node's transport; see the module documentation.
#[derive(Default)]
pub(crate) struct FrameLedger {
    /// Sends in progress: begun and not yet ended.
    sending: AtomicU64,
    /// Frames the transport committed to send; see the commit law.
    sent: AtomicU64,
    /// Frames that reached the link stage, counted before they are decoded.
    arrived: AtomicU64,
    /// Arrived frames whose lease is not yet released.
    in_flight: AtomicU64,
}

impl FrameLedger {
    /// Begin one send under `acceptance`; see [`FrameSend`] for when it counts as sent.
    pub(crate) fn begin_send(self: &Arc<Self>, acceptance: SendAcceptance) -> FrameSend {
        self.sending.fetch_add(1, Ordering::AcqRel);
        record_activity();
        FrameSend {
            ledger: Arc::clone(self),
            acceptance,
            accepted: false,
        }
    }

    /// Count one arrived frame; it stays in flight until the returned token is dropped.
    pub(crate) fn arrive(self: &Arc<Self>) -> FrameInFlight {
        // Sampling law: the witness before the count.
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        self.arrived.fetch_add(1, Ordering::AcqRel);
        record_activity();
        FrameInFlight {
            ledger: Arc::clone(self),
        }
    }

    /// Load the counters in the sampling law's order.
    pub(crate) fn sample(&self) -> FrameSample {
        let sending = self.sending.load(Ordering::Acquire);
        let sent = self.sent.load(Ordering::Acquire);
        let arrived = self.arrived.load(Ordering::Acquire);
        let in_flight = self.in_flight.load(Ordering::Acquire);
        FrameSample {
            sending,
            sent,
            arrived,
            in_flight,
        }
    }
}

/// Test builds: one [`FrameLedger::sample`] of a node's frame counts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameSample {
    /// Sends in progress.
    pub(crate) sending: u64,
    /// Frames counted as sent.
    pub(crate) sent: u64,
    /// Frames counted as arrived.
    pub(crate) arrived: u64,
    /// Arrived frames not yet handed off.
    pub(crate) in_flight: u64,
}

impl FrameSample {
    /// Whether a send is in progress or an arrived frame is not yet handed off.
    pub(crate) const fn busy(&self) -> bool {
        self.sending > 0 || self.in_flight > 0
    }
}

/// Test builds: one send in progress.
///
/// Dropped, it counts the frame as sent iff the transport committed to it (the commit law): the
/// send returned `Ok` ([`Self::accept`]) or its acceptance became irrevocable, after which the
/// transport may still deliver the frame although the send future is gone.
pub(crate) struct FrameSend {
    ledger: Arc<FrameLedger>,
    acceptance: SendAcceptance,
    accepted: bool,
}

impl FrameSend {
    /// The send returned `Ok`: count the frame as sent when the guard drops.
    pub(crate) fn accept(mut self) {
        self.accepted = true;
    }
}

impl Drop for FrameSend {
    /// End the send, counting the frame as sent first if the transport committed to it, so the
    /// ledger never shows a committed frame as neither sending nor sent.
    fn drop(&mut self) {
        if self.accepted || self.acceptance.is_irrevocable() {
            self.ledger.sent.fetch_add(1, Ordering::AcqRel);
        }
        self.ledger.sending.fetch_sub(1, Ordering::AcqRel);
        record_activity();
    }
}

/// Test builds: one arrived frame not yet handed off; see [`FrameLedger::arrive`].
pub(crate) struct FrameInFlight {
    ledger: Arc<FrameLedger>,
}

impl Drop for FrameInFlight {
    /// The frame was handed off or dropped: it is no longer in flight.
    fn drop(&mut self) {
        self.ledger.in_flight.fetch_sub(1, Ordering::AcqRel);
        record_activity();
    }
}
