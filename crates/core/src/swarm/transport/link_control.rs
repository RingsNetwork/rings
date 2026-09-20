//! Emission of link-control frames: the confirmations, questions and answers of the session
//! references on one link (see [`crate::swarm::session_link`]).
//!
//! A link-control frame is a link-layer signal, not a message: it is unsigned, idempotent,
//! belongs to no transaction, and is answered by nothing this end waits for. Three laws follow
//! for how it is sent.
//!
//! - Generation law: the frame is meaningful only on the connection generation it was judged
//!   on, since both ends' tables are per generation. It is sent on exactly that generation's
//!   connection and dropped when that generation is no longer current; it never leaves on a
//!   newer generation of the same peer, where it would confirm or disclaim what that generation's
//!   tables never saw.
//! - Detachment law: the sender does not wait for the data channel. The frames are emitted from
//!   the transport's inbound callback, which the read loop awaits; a send that waited there for
//!   the peer's receive window would stall this end's reading, and so the peer's sending, on a
//!   link both ends are still using. The send runs in its own task, bounded by the same
//!   accept timeout as every frame, and is judged as every frame is: a send that became
//!   irrevocable and timed out retires the connection through the transport's termination path,
//!   never by dropping the send.
//! - Bound law: nothing here reserves lane or memory capacity, so the rate of these frames is
//!   bounded only by the inbound frames that cause them: at most one confirmation or question
//!   per session slot of a frame this end verified or held, and one answer per question the
//!   peer asked, all on the authenticated link the cause arrived on.

use std::future::Future;

use bytes::Bytes;

use super::delivery::send_data_with_timeout;
use super::delivery::ChunkSendPermit;
use super::delivery::ChunkSendProgress;
use super::delivery::TransferStop;
use super::AdmittedConnection;
use super::PendingConnectionAttempt;
use super::SwarmTransport;
use crate::error::Error;
use crate::error::Result;
use crate::lifecycle::StopToken;
use crate::message::LinkControl;
use crate::session::SessionDigest;
use crate::utils::get_epoch_ms;

/// The send context every link-control frame is logged and judged under.
const LINK_CONTROL_SEND_CONTEXT: &str = "link_control";

/// Run `send` as a task of its own, or hand it back when no runtime can carry one, for the
/// caller to run inline.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
fn spawn_link_control_send<S>(send: S) -> Option<S>
where S: Future<Output = ()> + Send + 'static {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            drop(runtime.spawn(send));
            None
        }
        Err(_) => Some(send),
    }
}

/// Run `send` as a task of its own; the browser runtime always can.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
fn spawn_link_control_send<S>(send: S) -> Option<S>
where S: Future<Output = ()> + 'static {
    wasm_bindgen_futures::spawn_local(send);
    None
}

/// Hand `frame` to `admitted`'s data channel under the frame send discipline: cancelled while
/// still revocable if the generation is superseded or the transport cannot make progress,
/// retired through the termination path if it became irrevocable and timed out. The flush is
/// not awaited, since nothing depends on it.
async fn deliver_link_control(admitted: AdmittedConnection, frame: Bytes) {
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
}

impl SwarmTransport {
    /// Emit `control` on the connection generation `attempt`, without waiting for the send.
    ///
    /// Post: `Ok` means the frame is on its way on `attempt`'s connection, which was current
    /// when the send started; a superseded generation is refused as
    /// [`Error::ConnectionAttemptSuperseded`] and a peer with no admitted connection as
    /// [`Error::SwarmMissDidInTable`]. A runtime that cannot spawn runs the send inline.
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
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        super::outbound::record_emitted_link_control(peer, control);
        if let Some(inline) = spawn_link_control_send(deliver_link_control(admitted, frame)) {
            inline.await;
        }
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
