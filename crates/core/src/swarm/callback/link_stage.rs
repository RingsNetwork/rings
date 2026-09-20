//! The link stage of one inbound connection: the first thing a frame from the transport meets.
//!
//! A frame is a payload, whose session slots may be references that this connection's
//! [`ReferencedSessions`](crate::swarm::session_link::ReferencedSessions) resolves, or a
//! link-control frame, which asks about, confirms or answers such a reference. This module is
//! the imperative shell around that pure state: it reads the inbound clock, interprets the
//! effects the steps return (deliver, drop, ask, confirm), and hands every resolved frame to
//! [`InnerSwarmCallback::admit_resolved_frame`], where verification and admission are what they
//! were before references existed.
//!
//! Charging law: every frame this stage drops is charged to the peer as a receive failure, in
//! the same way a frame that fails verification is. Each such frame referenced a session the
//! peer did not back in time: the hold overflowed (the peer's references outran its answers),
//! the peer disclaimed or could not validly announce the session, or the frame waited past the
//! hold timeout (the sweep in
//! [`InboundProcessor::sweep_session_hold_at`](super::InboundProcessor::sweep_session_hold_at)).

use std::str::FromStr;

use bytes::Bytes;
use rings_transport::core::callback::InboundFrameCapacityLease;

use super::Completion;
use super::InboundFrameLease;
use super::InnerSwarmCallback;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use super::OnMessageRecursionDepthGuard;
use super::TransportCallbackError;
use crate::dht::Did;
use crate::error::Result;
use crate::message::LinkControl;
use crate::message::LinkFrame;
use crate::session::SessionDigest;
use crate::swarm::session_link::Digests;
use crate::swarm::session_link::FrameArrival;
use crate::swarm::transport::PendingConnectionAttempt;

impl InnerSwarmCallback {
    /// Take one frame from the transport through the link stage.
    ///
    /// ```text
    ///   bytes ─decode─┬─ LinkControl ─────▶ handle_link_control
    ///                 └─ Payload ─arrive─┬─ Resolved ─▶ admit_resolved_frame ─▶ confirm what it taught,
    ///                                    │                                     release what awaited it
    ///                                    ├─ Held ─────▶ ask the peer for what the frame misses
    ///                                    └─ Overflow ─▶ dropped and charged; the oldest held frame's
    ///                                                   question asked again
    /// ```
    pub(super) async fn submit_inbound_message(
        &self,
        cid: &str,
        msg: Bytes,
        transport_capacity: Option<InboundFrameCapacityLease>,
    ) -> std::result::Result<(), TransportCallbackError> {
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        let _depth_guard = OnMessageRecursionDepthGuard::enter();

        let peer = Did::from_str(cid).ok();
        let frame = match LinkFrame::from_wire(msg.as_ref()) {
            Ok(frame) => frame,
            Err(error) => {
                self.record_receive_failure_now(peer).await;
                return Err(error.into());
            }
        };
        let frame = match frame {
            LinkFrame::Control(control) => {
                self.handle_link_control(peer, control).await;
                return Ok(());
            }
            LinkFrame::Payload(frame) => frame,
        };
        let lease = InboundFrameLease {
            bytes: msg,
            transport_capacity,
        };
        let arrival = self
            .processor
            .session_link()
            .arrive(frame, lease, self.processor.now_ms());
        match arrival {
            Ok(FrameArrival::Resolved(resolved)) => {
                let learned = self
                    .admit_resolved_frame(peer, resolved, Completion::Awaited)
                    .await?;
                self.learned_sessions(peer, learned).await;
                Ok(())
            }
            Ok(FrameArrival::Held { request }) => {
                self.request_sessions(peer, request).await;
                Ok(())
            }
            Ok(FrameArrival::Overflow { carrier, request }) => {
                tracing::debug!(
                    peer = ?peer,
                    "dropping message; the hold for unresolved session references is full"
                );
                drop(carrier);
                self.record_receive_failure_now(peer).await;
                self.request_sessions(peer, request).await;
                Ok(())
            }
            Err(error) => {
                self.record_receive_failure_now(peer).await;
                Err(error.into())
            }
        }
    }

    /// Record a receive failure against `peer` under its authentication as of now.
    async fn record_receive_failure_now(&self, peer: Option<Did>) {
        let authentication = self.processor.authentication_of(peer);
        self.processor
            .record_receive_failure(peer, authentication)
            .await;
    }

    /// Deliver every held frame that resolves now, detached, so the transport's read loop is
    /// not paced by each one's handlers.
    ///
    /// A failure on one released frame is logged and does not stop the release: the frame was
    /// accepted from the transport when it arrived, and the others are independent of it. The
    /// loop re-scans the hold after each frame, so what a released frame taught is applied to
    /// the frames still held without recursing.
    async fn release_held_frames(&self, peer: Option<Did>) {
        loop {
            let release = self
                .processor
                .session_link()
                .release_next(self.processor.now_ms());
            let resolved = match release {
                Ok(Some(resolved)) => resolved,
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(peer = ?peer, error = ?error, "releasing a held message failed");
                    return;
                }
            };
            let learned = self
                .admit_resolved_frame(peer, resolved, Completion::Detached)
                .await;
            let learned = learned.unwrap_or_else(|error| {
                tracing::warn!(
                    peer = ?peer,
                    error = ?error,
                    "failed to deliver a message held for a session reference"
                );
                Digests::new()
            });
            self.confirm_sessions(peer, learned).await;
        }
    }

    /// The link learned `learned` from a frame that arrived: confirm them to `peer`, and let
    /// the held frames that awaited one of them leave.
    async fn learned_sessions(&self, peer: Option<Did>, learned: Digests) {
        let releases = !learned.is_empty();
        self.confirm_sessions(peer, learned).await;
        if releases {
            self.release_held_frames(peer).await;
        }
    }

    /// Tell `peer` that the sessions behind `confirm` verified here, so it may reference them.
    async fn confirm_sessions(&self, peer: Option<Did>, confirm: Digests) {
        let Some(peer) = peer else {
            return;
        };
        for digest in confirm {
            self.emit_link_control(peer, LinkControl::Known(digest))
                .await;
        }
    }

    /// Ask `peer` for the sessions behind `request`.
    async fn request_sessions(&self, peer: Option<Did>, request: Digests) {
        let Some(peer) = peer else {
            tracing::warn!("cannot ask an unparsable peer for a session");
            return;
        };
        for digest in request {
            self.emit_link_control(peer, LinkControl::Request(digest))
                .await;
        }
    }

    /// Send one control frame to `peer`; a failure is logged, since the next frame that misses
    /// or teaches the same session repeats the question or the confirmation.
    async fn emit_link_control(&self, peer: Did, control: LinkControl) {
        if let Err(error) = self
            .processor
            .logical
            .transport
            .send_link_control(peer, &control)
            .await
        {
            tracing::debug!(peer = %peer, error = ?error, control = ?control, "failed to send link control");
        }
    }

    /// Act on one link-control frame from `peer`.
    ///
    /// ```text
    ///   Known(d)    ─▶ mark d confirmed in the announced table of this connection generation
    ///   Request(d)  ─▶ answer from that table
    ///   Announce(s) ─▶ announce ─▶ fail refused frames ─▶ release what resolves
    ///   Unknown(d)  ─▶ unknown  ─▶ fail awaiting frames
    /// ```
    async fn handle_link_control(&self, peer: Option<Did>, control: LinkControl) {
        let Some(peer) = peer else {
            tracing::warn!("ignoring link control from an unparsable peer");
            return;
        };
        match control {
            LinkControl::Known(digest) => self.acknowledge_session(peer, digest),
            LinkControl::Request(digest) => self.answer_session_request(peer, digest).await,
            LinkControl::Announce(session) => {
                let refused = self
                    .processor
                    .session_link()
                    .announce(session, self.processor.now_ms());
                self.fail_unavailable(peer, refused).await;
                self.release_held_frames(Some(peer)).await;
            }
            LinkControl::Unknown(digest) => {
                let awaiting = self
                    .processor
                    .session_link()
                    .unknown(digest, self.processor.now_ms());
                self.fail_unavailable(peer, Ok(awaiting)).await;
            }
        }
    }

    /// Charge `peer` for every frame in `unavailable`: each awaited a session the peer could
    /// not, or did not validly, supply.
    async fn fail_unavailable(&self, peer: Did, unavailable: Result<Vec<InboundFrameLease>>) {
        match unavailable {
            Ok(unavailable) => {
                for _lease in unavailable {
                    tracing::warn!(
                        peer = %peer,
                        "dropping a message whose referenced session the peer could not supply"
                    );
                    self.record_receive_failure_now(Some(peer)).await;
                }
            }
            Err(error) => {
                tracing::warn!(peer = %peer, error = ?error, "failed to judge a session answer");
            }
        }
    }

    /// The connection generation this callback is bound to, if it is `peer`'s.
    fn bound_attempt(&self, peer: Did) -> Option<PendingConnectionAttempt> {
        self.pending_attempt()
            .filter(|attempt| attempt.peer() == peer)
    }

    /// `peer` confirmed `digest`: mark it in what this connection generation announced.
    fn acknowledge_session(&self, peer: Did, digest: SessionDigest) {
        let Some(attempt) = self.bound_attempt(peer) else {
            tracing::debug!(peer = %peer, "ignoring a session confirmation outside a bound connection");
            return;
        };
        self.processor
            .logical
            .transport
            .acknowledge_session(attempt, digest);
    }

    /// Answer `peer`'s question about `digest` from what this connection generation announced.
    /// A callback bound to no handshake, or to another peer's, has announced nothing to `peer`.
    async fn answer_session_request(&self, peer: Did, digest: SessionDigest) {
        let Some(attempt) = self.bound_attempt(peer) else {
            tracing::debug!(peer = %peer, "ignoring a session request outside a bound connection");
            return;
        };
        if let Err(error) = self
            .processor
            .logical
            .transport
            .answer_session_request(attempt, digest)
            .await
        {
            tracing::debug!(peer = %peer, error = ?error, "failed to answer a session request");
        }
    }
}
