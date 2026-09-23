//! Emission of link-control frames: the confirmations, questions and answers of the session
//! references on one link (see [`crate::swarm::session_link`]).
//!
//! A link-control frame is a link-layer signal, not a message: it is unsigned, idempotent,
//! belongs to no transaction, and is answered by nothing this end waits for. Three laws follow
//! for how it is sent.
//!
//! - Generation law: the frame is meaningful only on the connection generation it was judged
//!   on, since both ends' tables are per generation. It is sent on exactly that generation's
//!   connection and refused when that generation is no longer current; it never leaves on a
//!   newer generation of the same peer, where it would confirm or disclaim what that generation's
//!   tables never saw.
//! - Detachment law: the sender never waits for the data channel. The frames are emitted from
//!   the transport's inbound callback, which the read loop awaits; a send that waited there for
//!   the peer's receive window would stall this end's reading, and so the peer's sending, on a
//!   link both ends are still using. The send runs in a task of its own, refused when no
//!   runtime can carry one, bounded by the same accept timeout as every frame, and judged as
//!   every frame is: a send that became irrevocable and timed out retires the connection
//!   through the transport's termination path, never by dropping the send.
//! - Bound law: nothing here reserves lane or memory capacity. Each inbound frame causes at
//!   most two of these frames (a confirmation or question per session slot of a frame this
//!   end verified, held, or dropped for want of room, or one answer per question), and at
//!   most `LINK_CONTROL_IN_FLIGHT_CAPACITY` of them, twice the raw frames the peer may have in
//!   flight at this end's transport
//!   ([`INBOUND_PEER_FRAME_CAPACITY`](rings_transport::callback::INBOUND_PEER_FRAME_CAPACITY)),
//!   are in flight to one peer at a time: a
//!   send beyond that budget is refused, and repeated by the next frame that misses or
//!   teaches the same session. So what this end spends on a peer's link control is bounded
//!   by the frames it accepts from that peer, in flight and over time.
//!
//! The sender's table is judged on the system clock, like every outbound step; the receiver's
//! on the inbound clock. They are one clock outside tests.

use bytes::Bytes;

use super::delivery::send_data_with_timeout;
use super::delivery::ChunkSendPermit;
use super::delivery::ChunkSendProgress;
use super::delivery::TransferStop;
use super::outbound::LinkControlPermit;
use super::AdmittedConnection;
use super::PendingConnectionAttempt;
use super::SwarmTransport;
use crate::error::Error;
use crate::error::Result;
use crate::lifecycle::StopToken;
use crate::message::LinkControl;
use crate::session::SessionDigest;
use crate::swarm::detached::spawn_detached;
use crate::utils::get_epoch_ms;

/// The send context every link-control frame is logged and judged under.
const LINK_CONTROL_SEND_CONTEXT: &str = "link_control";

/// Hand `frame` to `admitted`'s data channel under the frame send discipline: cancelled while
/// still revocable if the generation is superseded or the transport cannot make progress,
/// retired through the termination path if it became irrevocable and timed out. The flush is
/// not awaited, since nothing depends on it; `permit` returns to the peer's budget when the
/// send is over.
async fn deliver_link_control(
    admitted: AdmittedConnection,
    frame: Bytes,
    permit: LinkControlPermit,
) {
    let peer = admitted.attempt().peer();
    let stop = TransferStop::new(StopToken::never());
    let progress = send_data_with_timeout(
        &admitted,
        frame,
        &ChunkSendPermit::Always,
        &stop,
        None,
        peer,
        LINK_CONTROL_SEND_CONTEXT,
    )
    .await;
    match progress {
        ChunkSendProgress::Ready(Ok(_flush)) => {}
        ChunkSendProgress::Ready(Err(error)) => {
            tracing::debug!(peer = %peer, error = ?error, "failed to send link control");
        }
        ChunkSendProgress::Cancelled(reason) => {
            tracing::debug!(peer = %peer, reason = ?reason, "link control send cancelled");
        }
    }
    drop(permit);
}

impl SwarmTransport {
    /// Emit `control` on the connection generation `attempt`, without waiting for the send.
    ///
    /// Post: `Ok` means the frame was handed to a task of its own with `attempt`'s connection,
    /// which was current when the task started. Refused as
    /// [`Error::ConnectionAttemptSuperseded`] for a generation no longer current,
    /// [`Error::SwarmMissDidInTable`] for a peer with no admitted connection,
    /// [`Error::LinkControlInFlightCapacity`] when the peer's budget is spent, and
    /// [`Error::LinkControlRuntimeUnavailable`] when no runtime can carry the task.
    pub(crate) async fn send_link_control(
        &self,
        attempt: PendingConnectionAttempt,
        control: &LinkControl,
    ) -> Result<()> {
        let peer = attempt.peer();
        let frame = control.to_wire()?;
        let Some(admitted) = self.admitted_send_connection(peer)? else {
            return Err(Error::SwarmMissDidInTable(peer));
        };
        if admitted.attempt() != attempt {
            return Err(Error::ConnectionAttemptSuperseded {
                peer,
                generation: attempt.generation(),
            });
        }
        // The permit is taken while the generation cannot be retired, so no worker is looked
        // up for a peer that retirement is removing.
        let permit = admitted
            .with_current_connection(|_| self.outbound_schedulers.link_control_permit(peer))?;
        let permit = match permit {
            None => {
                return Err(Error::ConnectionAttemptSuperseded {
                    peer,
                    generation: attempt.generation(),
                })
            }
            Some(None) => return Err(Error::SwarmMissDidInTable(peer)),
            Some(Some(None)) => return Err(Error::LinkControlInFlightCapacity(peer)),
            Some(Some(Some(permit))) => permit,
        };
        if spawn_detached(Box::pin(deliver_link_control(admitted, frame, permit))).is_err() {
            return Err(Error::LinkControlRuntimeUnavailable);
        }
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        super::outbound::record_dispatched_link_control(peer, control);
        Ok(())
    }

    /// `attempt`'s peer confirmed `digest`: frames to it may reference the session from now on.
    pub(crate) fn acknowledge_session(
        &self,
        attempt: PendingConnectionAttempt,
        digest: SessionDigest,
    ) {
        self.outbound_schedulers
            .acknowledge_session(attempt.peer(), attempt.generation(), digest);
    }

    /// Answer the question `attempt`'s peer asked about `digest`: exactly one link-control
    /// frame per question, taken from what this generation announced, sent on this generation.
    pub(crate) async fn answer_session_request(
        &self,
        attempt: PendingConnectionAttempt,
        digest: SessionDigest,
    ) -> Result<()> {
        let answer = self.outbound_schedulers.answer_session_request(
            attempt.peer(),
            attempt.generation(),
            digest,
            get_epoch_ms(),
        );
        self.send_link_control(attempt, &answer).await
    }
}
