//! The link stage of one inbound connection: the first thing a frame from the transport meets.
//!
//! A frame is a payload, whose delegation slots may be references that this connection's
//! [`ReferencedDelegations`](crate::swarm::session_link::ReferencedDelegations) resolves, or a
//! link-control frame, which asks about, confirms or answers such a reference. This module is
//! the imperative shell around that pure state: it reads the inbound clock, interprets the
//! effects the steps return (deliver, drop, ask, confirm), and hands every resolved frame to
//! [`InnerSwarmCallback::verify_frame`] and then [`InnerSwarmCallback::gate_prepared_frame`],
//! where verification and admission are what they were before references existed.
//!
//! Link law: a reference resolves only on a link, and this callback's link is the handshake it
//! is bound to, with that handshake's peer, on that handshake's connection generation. Every
//! question, confirmation and answer names that generation and leaves on it alone (see
//! [`SwarmTransport::send_link_control`](crate::swarm::transport::SwarmTransport::send_link_control)),
//! only frames on the link teach the link's table, and every frame in the hold was admitted
//! under the link's peer, so every drop charges one identity. A frame from any other peer, or
//! on a callback bound to no handshake, has no link: it is judged self-contained, as it would
//! be outside any connection, a reference in it is refused as
//! [`Error::DelegationReferenceUnresolved`](crate::error::Error::DelegationReferenceUnresolved), and
//! nothing it carries reaches the table.
//!
//! Learning law: what a verified frame teaches the link is confirmed to the peer, and the held
//! frames that awaited it released, before the frame itself is gated: a held frame never
//! waits on the fate of the frame that taught it. A release runs in a task of its own, so it
//! does not pace the transport's read loop; only when no runtime is current does it run inline,
//! since a release omitted would leave frames held for a session the link knows (see
//! [`run_detached_or_inline`]).
//!
//! Charging law: a frame this stage drops because the peer did not back a session it
//! referenced is charged to the peer as a receive failure, in the same way a frame that fails
//! verification is: the peer disclaimed the session, announced a delegation that does not
//! verify, or let the frame wait past the hold timeout (the sweep in
//! [`InboundProcessor::sweep_session_hold_at`](super::InboundProcessor::sweep_session_hold_at)),
//! or the held frame failed to resolve or verify on release. A frame dropped because the hold
//! is full is a loss at this end's capacity, as a frame the pre-admission hold cannot take is,
//! and is not charged: the peer has not yet had its round trip to answer.

use std::str::FromStr;

use bytes::Bytes;
use rings_transport::core::callback::InboundFrameCapacityLease;

use super::processor::HeldFrameDrop;
use super::FrameProvenance;
use super::InboundFrameLease;
use super::InnerSwarmCallback;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use super::OnMessageRecursionDepthGuard;
use super::TransportCallbackError;
use crate::delegation::DelegationDigest;
use crate::dht::Did;
use crate::message::LinkControl;
use crate::message::LinkFrame;
use crate::message::WirePayload;
use crate::swarm::detached::run_detached_or_inline;
use crate::swarm::detached::DetachedTask;
use crate::swarm::session_link::Announcement;
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
    ///                             └─ link ─arrive─┬─ Resolved ─▶ verify ─▶ learn ─▶ confirm what it
    ///                                             │              taught, release what awaited it ─▶ gate
    ///                                             ├─ Held ─────▶ ask the peer for what the frame misses
    ///                                             └─ Overflow ─▶ dropped, uncharged; the oldest held
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
                self.processor.record_receive_failure_now(peer).await;
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
        let Some(link) = self.bound_attempt(peer) else {
            return self.submit_off_link(peer, frame, lease).await;
        };
        let arrival = self
            .processor
            .session_link()
            .arrive(frame, lease, self.processor.now_ms());
        match arrival {
            Ok(FrameArrival::Resolved(resolved)) => {
                self.admit_on_link(link, resolved, FrameProvenance::Arrived)
                    .await
            }
            Ok(FrameArrival::Held { request }) => {
                let asked = self.request_delegations(link, request).await;
                self.processor.session_link().note_asked(asked);
                Ok(())
            }
            Ok(FrameArrival::Overflow { carrier, request }) => {
                tracing::debug!(
                    peer = %link.peer(),
                    "dropping message; the hold for unresolved delegation references is full"
                );
                // Dropping the carrier is the effect: it releases the frame's transport lease.
                drop(carrier);
                let asked = self.request_delegations(link, request).await;
                self.processor.session_link().note_asked(asked);
                Ok(())
            }
            Err(error) => {
                self.processor.record_receive_failure_now(peer).await;
                Err(error.into())
            }
        }
    }

    /// Verify one frame resolved on `link`, let the link learn the sessions it carried inline,
    /// confirm them, release what awaited them if the frame arrived (a released frame's
    /// learning is re-scanned by the release that freed it), then gate the frame.
    async fn admit_on_link(
        &self,
        link: PendingConnectionAttempt,
        resolved: Box<ResolvedFrame<InboundFrameLease>>,
        provenance: FrameProvenance,
    ) -> std::result::Result<(), TransportCallbackError> {
        let peer = Some(link.peer());
        let ResolvedFrame {
            payload,
            carrier: lease,
            encoding,
        } = *resolved;
        let prepared = self.verify_frame(peer, payload, lease.bytes.len()).await?;
        let (learned, releases) = {
            let now_ms = self.processor.now_ms();
            let mut session_link = self.processor.session_link();
            let learned = session_link.admit_verified(&prepared.payload, encoding, now_ms)?;
            let releases =
                provenance.releases_what_it_teaches() && session_link.awaits_any(&learned, now_ms);
            (learned, releases)
        };
        self.confirm_delegations(link, learned).await;
        if releases {
            self.start_release(link).await;
        }
        self.gate_prepared_frame(peer, prepared, lease, provenance)
            .await
    }

    /// Admit a payload frame that arrived on no link: it must be self-contained, and nothing
    /// is learned or confirmed from it, since there is no link to learn for.
    async fn submit_off_link(
        &self,
        peer: Option<Did>,
        frame: Box<WirePayload<'static>>,
        lease: InboundFrameLease,
    ) -> std::result::Result<(), TransportCallbackError> {
        let payload = match frame.into_self_contained() {
            Ok(payload) => payload,
            Err(error) => {
                self.processor.record_receive_failure_now(peer).await;
                return Err(error.into());
            }
        };
        let prepared = self.verify_frame(peer, payload, lease.bytes.len()).await?;
        self.gate_prepared_frame(peer, prepared, lease, FrameProvenance::Arrived)
            .await
    }

    /// Release every held frame that resolves now, detached from the caller, or inline when no
    /// runtime is current (see [`run_detached_or_inline`]).
    async fn start_release(&self, link: PendingConnectionAttempt) {
        run_detached_or_inline(self.release_task(link)).await;
    }

    /// The release of every held frame that resolves now, as a task of its own. Built in a
    /// plain function, not inside `start_release`'s future: a release admits frames that may
    /// start a release, and boxing here is what keeps the `Send` proof of the task from
    /// recursing through the future that would otherwise contain it.
    fn release_task(&self, link: PendingConnectionAttempt) -> DetachedTask {
        let releaser = self.detached_handle();
        Box::pin(async move {
            releaser.release_held_frames(link).await;
        })
    }

    /// Deliver every held frame that resolves now, detached from its logical completion.
    ///
    /// A frame that fails on release is charged and skipped, and does not stop the release:
    /// the others are independent of it. The loop re-scans the hold after each frame, so what
    /// a released frame taught is applied to the frames still held without recursing. Two
    /// releases running at once each take frames the other has not, in arrival order.
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
                    // Unreachable by construction: a frame is released only once every slot
                    // resolves under the same lock. Reported, never charged, since the frame is
                    // already out of the hold.
                    tracing::error!(
                        peer = %link.peer(),
                        error = ?error,
                        "a released message failed to resolve; the hold's invariant is broken"
                    );
                    continue;
                }
            };
            if let Err(error) = self
                .admit_on_link(link, resolved, FrameProvenance::Released)
                .await
            {
                tracing::warn!(
                    peer = %link.peer(),
                    error = ?error,
                    "failed to deliver a message held for a delegation reference"
                );
            }
        }
    }

    /// Tell the peer that the sessions behind `confirm` verified here, so it may reference them.
    async fn confirm_delegations(&self, link: PendingConnectionAttempt, confirm: Digests) {
        for digest in confirm {
            let _dispatched = self
                .emit_link_control(link, LinkControl::Known(digest))
                .await;
        }
    }

    /// Ask the peer for the sessions behind `request`. Post: the digests actually asked about,
    /// for the hold to know which frames the peer may be charged for.
    async fn request_delegations(
        &self,
        link: PendingConnectionAttempt,
        request: Digests,
    ) -> Digests {
        let mut asked = Digests::new();
        for digest in request {
            if self
                .emit_link_control(link, LinkControl::Request(digest))
                .await
            {
                asked.insert(digest);
            }
        }
        asked
    }

    /// Emit one control frame on `link`. Post: whether it was dispatched; a refusal is logged,
    /// since the next frame that misses or teaches the same session repeats the question or
    /// the confirmation.
    async fn emit_link_control(
        &self,
        link: PendingConnectionAttempt,
        control: LinkControl,
    ) -> bool {
        match self
            .processor
            .logical
            .transport
            .send_link_control(link, &control)
            .await
        {
            Ok(()) => true,
            Err(error) => {
                tracing::debug!(
                    peer = %link.peer(),
                    error = ?error,
                    control = ?control,
                    "link control not sent"
                );
                false
            }
        }
    }

    /// Act on one link-control frame from `peer`. A frame from a peer this callback is not
    /// bound to is on no link and changes nothing.
    ///
    /// ```text
    ///   Known(d)    ─▶ mark d confirmed in the announced table of this connection generation
    ///   Request(d)  ─▶ answer from that table
    ///   Announce(s) ─▶ announce ─▶ charge refused frames ─▶ release what resolves
    ///   Unknown(d)  ─▶ unknown  ─▶ charge awaiting frames
    /// ```
    async fn handle_link_control(&self, peer: Option<Did>, control: LinkControl) {
        let Some(link) = self.bound_attempt(peer) else {
            tracing::debug!(peer = ?peer, "ignoring link control outside a bound connection");
            return;
        };
        match control {
            LinkControl::Known(digest) => self
                .processor
                .logical
                .transport
                .acknowledge_session(link, digest),
            LinkControl::Request(digest) => self.answer_session_request(link, digest).await,
            LinkControl::Announce(session) => {
                let verdict = self
                    .processor
                    .session_link()
                    .announce(session, self.processor.now_ms());
                match verdict {
                    Ok(Announcement::Ignored) => {}
                    Ok(Announcement::Admitted) => self.start_release(link).await,
                    Ok(Announcement::Refused(refused)) => {
                        self.processor
                            .charge_dropped_frames(
                                Some(link.peer()),
                                refused,
                                HeldFrameDrop::AnnouncementRefused,
                            )
                            .await;
                    }
                    Err(error) => tracing::warn!(
                        peer = %link.peer(),
                        error = ?error,
                        "failed to judge a session announcement"
                    ),
                }
            }
            LinkControl::Unknown(digest) => {
                let awaiting = self
                    .processor
                    .session_link()
                    .unknown(digest, self.processor.now_ms());
                self.processor
                    .charge_dropped_frames(Some(link.peer()), awaiting, HeldFrameDrop::Disclaimed)
                    .await;
            }
        }
    }

    /// The link of a frame from `peer`: the handshake this callback is bound to, if it is
    /// `peer`'s. A production callback is bound to the handshake of the connection it serves,
    /// so the filter holds by construction there; it is the stated law, not a defence.
    fn bound_attempt(&self, peer: Option<Did>) -> Option<PendingConnectionAttempt> {
        let peer = peer?;
        self.pending_attempt()
            .filter(|attempt| attempt.is_with(peer))
    }

    /// Answer the peer's question about `digest` from what `link`'s generation announced.
    async fn answer_session_request(
        &self,
        link: PendingConnectionAttempt,
        digest: DelegationDigest,
    ) {
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
                "session request not answered"
            );
        }
    }
}
