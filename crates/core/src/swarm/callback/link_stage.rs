//! The link stage of one inbound connection: the first thing a frame from the transport meets.
//!
//! A frame is a payload, whose session slots may be references that this connection's
//! [`ReferencedSessions`](crate::swarm::session_link::ReferencedSessions) resolves, or a
//! link-control frame, which asks about, confirms or answers such a reference. This module is
//! the imperative shell around that pure state: it reads the inbound clock, interprets the
//! effects the steps return (deliver, drop, ask, confirm), and hands every resolved frame to
//! [`InnerSwarmCallback::verify_resolved_frame`] and then
//! [`InnerSwarmCallback::gate_prepared_frame`], where verification and admission are what they
//! were before references existed.
//!
//! Link law: a reference resolves only on a link, and this callback's link is the handshake it
//! is bound to, with that handshake's peer, on that handshake's connection generation. Every
//! question, confirmation and answer names that generation and leaves on it alone (see
//! [`SwarmTransport::send_link_control`](crate::swarm::transport::SwarmTransport::send_link_control)),
//! and every frame in the hold was admitted under that peer, so every drop charges one identity.
//! A frame from any other peer, or on a callback bound to no handshake, has no link: it is
//! judged self-contained, as it would be outside any connection, and a reference in it is
//! refused as [`Error::SessionReferenceUnresolved`].
//!
//! Charging law: every frame this stage drops is charged to the peer as a receive failure, in
//! the same way a frame that fails verification is. Each such frame referenced a session the
//! peer did not back in time: the hold overflowed (the peer's references outran its answers),
//! the peer disclaimed or could not validly announce the session, or the frame waited past the
//! hold timeout (the sweep in
//! [`InboundProcessor::sweep_session_hold_at`](super::InboundProcessor::sweep_session_hold_at)).
//! A frame released from the hold is charged by the stage that then drops it, if any, as a
//! frame that never waited would be.

use std::str::FromStr;

use bytes::Bytes;
use rings_transport::core::callback::InboundFrameCapacityLease;

use super::inner::VerifiedFrame;
use super::InboundFrameLease;
use super::InnerSwarmCallback;
use super::LogicalCompletion;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use super::OnMessageRecursionDepthGuard;
use super::TransportCallbackError;
use crate::dht::Did;
use crate::message::LinkControl;
use crate::message::LinkFrame;
use crate::message::PerSlot;
use crate::message::SlotEncoding;
use crate::message::WirePayload;
use crate::session::SessionDigest;
use crate::swarm::session_link::Digests;
use crate::swarm::session_link::FrameArrival;
use crate::swarm::session_link::ResolvedFrame;
use crate::swarm::transport::PendingConnectionAttempt;

impl InnerSwarmCallback {
    /// Take one frame from the transport through the link stage.
    ///
    /// ```text
    ///   bytes ─decode─┬─ LinkControl ─────▶ handle_link_control
    ///                 └─ Payload ─┬─ no link ─▶ self-contained ─▶ verify ─▶ gate
    ///                             └─ link ─arrive─┬─ Resolved ─▶ verify ─▶ confirm what it taught,
    ///                                             │              release what awaited it ─▶ gate
    ///                                             ├─ Held ─────▶ ask the peer for what the frame misses
    ///                                             └─ Overflow ─▶ dropped and charged; the oldest held
    ///                                                            frame's question asked again
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
        let Some(link) = peer.and_then(|peer| self.bound_attempt(peer)) else {
            return self.submit_off_link(peer, frame, lease).await;
        };
        let arrival = self
            .processor
            .session_link()
            .arrive(frame, lease, self.processor.now_ms());
        match arrival {
            Ok(FrameArrival::Resolved(resolved)) => {
                let VerifiedFrame {
                    prepared,
                    lease,
                    learned,
                } = self.verify_resolved_frame(peer, resolved).await?;
                self.learned_sessions(link, learned).await;
                self.gate_prepared_frame(peer, prepared, lease, LogicalCompletion::Awaited)
                    .await
            }
            Ok(FrameArrival::Held { request }) => {
                self.request_sessions(link, request).await;
                Ok(())
            }
            Ok(FrameArrival::Overflow { carrier, request }) => {
                tracing::debug!(
                    peer = %link.peer(),
                    "dropping message; the hold for unresolved session references is full"
                );
                // Dropping the carrier is the effect: it releases the frame's transport lease.
                drop(carrier);
                self.record_receive_failure_now(peer).await;
                self.request_sessions(link, request).await;
                Ok(())
            }
            Err(error) => {
                self.record_receive_failure_now(peer).await;
                Err(error.into())
            }
        }
    }

    /// Admit a payload frame that arrived on no link: it must be self-contained, and nothing
    /// is confirmed for it, since there is no link to confirm on.
    async fn submit_off_link(
        &self,
        peer: Option<Did>,
        frame: Box<WirePayload<'static>>,
        lease: InboundFrameLease,
    ) -> std::result::Result<(), TransportCallbackError> {
        let payload = match frame.into_self_contained() {
            Ok(payload) => payload,
            Err(error) => {
                self.record_receive_failure_now(peer).await;
                return Err(error.into());
            }
        };
        let resolved = Box::new(ResolvedFrame {
            payload,
            carrier: lease,
            encoding: PerSlot {
                origin: SlotEncoding::Inline,
                hop: SlotEncoding::Inline,
            },
        });
        let VerifiedFrame {
            prepared,
            lease,
            learned: _unconfirmed,
        } = self.verify_resolved_frame(peer, resolved).await?;
        self.gate_prepared_frame(peer, prepared, lease, LogicalCompletion::Awaited)
            .await
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
    async fn release_held_frames(&self, link: PendingConnectionAttempt) {
        loop {
            let release = self
                .processor
                .session_link()
                .release_next(self.processor.now_ms());
            let resolved = match release {
                Ok(Some(resolved)) => resolved,
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(
                        peer = %link.peer(),
                        error = ?error,
                        "releasing a held message failed"
                    );
                    return;
                }
            };
            let peer = Some(link.peer());
            let verified = match self.verify_resolved_frame(peer, resolved).await {
                Ok(verified) => verified,
                Err(error) => {
                    tracing::warn!(
                        peer = %link.peer(),
                        error = ?error,
                        "a message held for a session reference failed verification"
                    );
                    continue;
                }
            };
            let VerifiedFrame {
                prepared,
                lease,
                learned,
            } = verified;
            self.confirm_sessions(link, learned).await;
            if let Err(error) = self
                .gate_prepared_frame(peer, prepared, lease, LogicalCompletion::Detached)
                .await
            {
                tracing::warn!(
                    peer = %link.peer(),
                    error = ?error,
                    "failed to deliver a message held for a session reference"
                );
            }
        }
    }

    /// The link learned `learned` from a frame that arrived: confirm them to the peer, and let
    /// the held frames that awaited one of them leave.
    async fn learned_sessions(&self, link: PendingConnectionAttempt, learned: Digests) {
        let releases = !learned.is_empty();
        self.confirm_sessions(link, learned).await;
        if releases {
            self.release_held_frames(link).await;
        }
    }

    /// Tell the peer that the sessions behind `confirm` verified here, so it may reference them.
    async fn confirm_sessions(&self, link: PendingConnectionAttempt, confirm: Digests) {
        for digest in confirm {
            self.emit_link_control(link, LinkControl::Known(digest))
                .await;
        }
    }

    /// Ask the peer for the sessions behind `request`.
    async fn request_sessions(&self, link: PendingConnectionAttempt, request: Digests) {
        for digest in request {
            self.emit_link_control(link, LinkControl::Request(digest))
                .await;
        }
    }

    /// Emit one control frame on `link`; a failure is logged, since the next frame that misses
    /// or teaches the same session repeats the question or the confirmation.
    async fn emit_link_control(&self, link: PendingConnectionAttempt, control: LinkControl) {
        if let Err(error) = self
            .processor
            .logical
            .transport
            .send_link_control(link, &control)
            .await
        {
            tracing::debug!(
                peer = %link.peer(),
                error = ?error,
                control = ?control,
                "failed to send link control"
            );
        }
    }

    /// Act on one link-control frame from `peer`. A frame from a peer this callback is not
    /// bound to is on no link and changes nothing.
    ///
    /// ```text
    ///   Known(d)    ─▶ mark d confirmed in the announced table of this connection generation
    ///   Request(d)  ─▶ answer from that table
    ///   Announce(s) ─▶ announce ─▶ fail refused frames ─▶ release what resolves
    ///   Unknown(d)  ─▶ unknown  ─▶ fail awaiting frames
    /// ```
    async fn handle_link_control(&self, peer: Option<Did>, control: LinkControl) {
        let Some(link) = peer.and_then(|peer| self.bound_attempt(peer)) else {
            tracing::debug!(peer = ?peer, "ignoring link control outside a bound connection");
            return;
        };
        match control {
            LinkControl::Known(digest) => self.acknowledge_session(link, digest),
            LinkControl::Request(digest) => self.answer_session_request(link, digest).await,
            LinkControl::Announce(session) => {
                let refused = self
                    .processor
                    .session_link()
                    .announce(session, self.processor.now_ms());
                match refused {
                    Ok(refused) => self.fail_unavailable(link, refused).await,
                    Err(error) => tracing::warn!(
                        peer = %link.peer(),
                        error = ?error,
                        "failed to judge a session announcement"
                    ),
                }
                self.release_held_frames(link).await;
            }
            LinkControl::Unknown(digest) => {
                let awaiting = self
                    .processor
                    .session_link()
                    .unknown(digest, self.processor.now_ms());
                self.fail_unavailable(link, awaiting).await;
            }
        }
    }

    /// Charge the peer for every frame in `unavailable`: each awaited a session the peer could
    /// not, or did not validly, supply.
    async fn fail_unavailable(
        &self,
        link: PendingConnectionAttempt,
        unavailable: Vec<InboundFrameLease>,
    ) {
        for _lease in unavailable {
            tracing::warn!(
                peer = %link.peer(),
                "dropping a message whose referenced session the peer could not supply"
            );
            self.record_receive_failure_now(Some(link.peer())).await;
        }
    }

    /// The link of a frame from `peer`: the handshake this callback is bound to, if it is
    /// `peer`'s. A production callback is bound to the handshake of the connection it serves,
    /// so the filter holds by construction there; it is the stated law, not a defence.
    fn bound_attempt(&self, peer: Did) -> Option<PendingConnectionAttempt> {
        self.pending_attempt()
            .filter(|attempt| attempt.peer() == peer)
    }

    /// The peer confirmed `digest`: mark it in what `link`'s generation announced.
    fn acknowledge_session(&self, link: PendingConnectionAttempt, digest: SessionDigest) {
        self.processor
            .logical
            .transport
            .acknowledge_session(link, digest);
    }

    /// Answer the peer's question about `digest` from what `link`'s generation announced.
    async fn answer_session_request(&self, link: PendingConnectionAttempt, digest: SessionDigest) {
        if let Err(error) = self
            .processor
            .logical
            .transport
            .answer_session_request(link, digest)
            .await
        {
            tracing::debug!(
                peer = %link.peer(),
                error = ?error,
                "failed to answer a session request"
            );
        }
    }
}
