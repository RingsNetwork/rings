//! The sessions this end has announced to one peer: the table the outbound worker encodes
//! frames against, shared with the inbound side that answers the peer's questions about it.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use bytes::Bytes;
use rings_transport::core::transport::FrameDelivery;

use super::transfer::OutboundFrame;
use super::OutboundSchedulers;
use crate::dht::Did;
use crate::error::Result;
use crate::message::SessionControl;
use crate::message::WirePayload;
use crate::session::SessionDigest;
use crate::swarm::session_link::AnnouncedSessions;
use crate::swarm::session_link::FramePlan;

/// The link a frame is about to be sent on: the connection generation its table belongs to, and
/// what the transport guarantees about the frames it accepts.
#[derive(Clone, Copy)]
pub(super) struct OutboundLink {
    pub(super) generation: u64,
    pub(super) delivery: FrameDelivery,
}

/// The one handle on a peer's [`AnnouncedSessions`]: every access is one pure step under the
/// lock.
///
/// Lock law: the lock is never held across a suspension point, and a poisoned lock still guards
/// a well-formed table, since no step panics between two writes. Clone law: clones name the
/// same table.
#[derive(Clone)]
pub(super) struct SharedAnnouncedSessions(Arc<Mutex<AnnouncedSessions>>);

impl SharedAnnouncedSessions {
    /// The table of a peer nothing has been sent to.
    pub(super) fn new() -> Self {
        Self(Arc::new(Mutex::new(AnnouncedSessions::new())))
    }

    /// The table, for one step.
    fn lock(&self) -> MutexGuard<'_, AnnouncedSessions> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The bytes of `frame` on `link` at `now_ms`, and what sending them announces.
    ///
    /// ```text
    ///   SessionControl(bytes) ──────────────▶ (bytes, nothing announced)
    ///   Payload over Unsequenced ────────▶ (inline encoding, nothing announced)
    ///   Payload over Sequenced ──plan────▶ (encoding with the planned references, the plan)
    /// ```
    ///
    /// A reference relies on an earlier frame having arrived, which only sequenced delivery
    /// promises; elsewhere every frame stays self-contained and the table is left alone. The
    /// worker calls this immediately before the send, so the order of these decisions is the
    /// order of acceptance the table's soundness law needs.
    pub(super) fn encode(
        &self,
        link: OutboundLink,
        frame: OutboundFrame,
        now_ms: u128,
    ) -> Result<(Bytes, Option<FramePlan>)> {
        match (frame, link.delivery) {
            (OutboundFrame::SessionControl(bytes), _) => Ok((bytes, None)),
            (OutboundFrame::Payload(payload), FrameDelivery::Unsequenced) => {
                payload.to_wire().map(|bytes| (bytes, None))
            }
            (OutboundFrame::Payload(payload), FrameDelivery::Sequenced) => {
                let payload = payload.as_ref();
                let plan = self.lock().plan(link.generation, payload, now_ms)?;
                let bytes = WirePayload::view(payload, plan.session_refs(payload)).to_wire()?;
                Ok((bytes, Some(plan)))
            }
        }
    }

    /// Record that the frame planned as `plan` was accepted by the transport at `now_ms`: only
    /// then is it on the link, and only then has it announced anything.
    pub(super) fn commit(&self, plan: FramePlan, now_ms: u128) {
        self.lock().commit(plan, now_ms);
    }
}

impl OutboundSchedulers {
    /// Answer `peer`'s question about `digest` on the link of `generation` at `now_ms`. A peer
    /// this end has no scheduler for was sent nothing, so nothing is known to it.
    pub(in crate::swarm::transport) fn answer_session_request(
        &self,
        peer: Did,
        generation: u64,
        digest: SessionDigest,
        now_ms: u128,
    ) -> SessionControl {
        let handle = self.lock_registry().peers.get(&peer).cloned();
        let answer = handle.map_or(SessionControl::Unknown(digest), |handle| {
            handle
                .state
                .announced
                .lock()
                .answer(generation, digest, now_ms)
        });
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        super::test_trace::record_session_answer(&answer);
        answer
    }
}
