//! Test builds: frame conservation of one node's transport, for the test harness's quiescence.
//!
//! Every frame a node exchanges with another node crosses two chokepoints: it leaves through
//! `SwarmConnection::send_data` and enters through the link stage's `submit_inbound_message`.
//! The ledger counts both, at the frame's first and last moment inside this crate:
//!
//! ```text
//!   send_data ─ sending ↑ ─ transport accepts ─ sent ↑, sending ↓
//!                                  ┆ the wire, a transport's delay
//!   submit_inbound_message ─ arrived ↑, in_flight ↑ ─ decode ─ … ─ handoff ─ in_flight ↓
//! ```
//!
//! Conservation law: over all nodes of a test, a frame is on the wire exactly while it is counted
//! as sent and not as arrived. Hence, with nothing sending,
//! `Σ sent = Σ arrived ⇔ no frame is on the wire`, unless a frame was lost below the swarm.
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

use crate::tests::activity::record_activity;

/// Test builds: the frame counts of one node's transport; see the module documentation.
#[derive(Default)]
pub(crate) struct FrameLedger {
    /// Sends in progress: begun and not yet accepted or failed by the transport.
    sending: AtomicU64,
    /// Frames the transport accepted for sending.
    sent: AtomicU64,
    /// Frames that reached the link stage, counted before they are decoded.
    arrived: AtomicU64,
    /// Arrived frames whose lease is not yet released.
    in_flight: AtomicU64,
}

impl FrameLedger {
    /// Begin one send; the frame is counted as sent only if the returned guard is committed.
    pub(crate) fn begin_send(self: &Arc<Self>) -> FrameSend {
        self.sending.fetch_add(1, Ordering::AcqRel);
        record_activity();
        FrameSend {
            ledger: Arc::clone(self),
            accepted: false,
        }
    }

    /// Count one arrived frame; it stays in flight until the returned token is dropped.
    pub(crate) fn arrive(self: &Arc<Self>) -> FrameInFlight {
        self.arrived.fetch_add(1, Ordering::AcqRel);
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        record_activity();
        FrameInFlight {
            ledger: Arc::clone(self),
        }
    }

    /// Whether a send is in progress or an arrived frame is not yet handed off.
    pub(crate) fn busy(&self) -> bool {
        self.sending.load(Ordering::Acquire) > 0 || self.in_flight.load(Ordering::Acquire) > 0
    }

    /// Frames the transport accepted for sending.
    pub(crate) fn sent(&self) -> u64 {
        self.sent.load(Ordering::Acquire)
    }

    /// Frames that reached the link stage.
    pub(crate) fn arrived(&self) -> u64 {
        self.arrived.load(Ordering::Acquire)
    }
}

/// Test builds: one send in progress; dropped uncommitted, the frame was not sent.
pub(crate) struct FrameSend {
    ledger: Arc<FrameLedger>,
    accepted: bool,
}

impl FrameSend {
    /// The transport accepted the frame: count it as sent when the guard drops.
    pub(crate) fn accept(mut self) {
        self.accepted = true;
    }
}

impl Drop for FrameSend {
    /// End the send, counting the frame as sent first if it was accepted, so the ledger never
    /// shows the frame as neither sending nor sent.
    fn drop(&mut self) {
        if self.accepted {
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
