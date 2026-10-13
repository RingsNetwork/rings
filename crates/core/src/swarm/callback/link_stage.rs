//! The link stage of one inbound connection: the first thing a frame from the transport meets.
//!
//! A frame is a payload, whose delegation slots may be references that this connection's
//! [`ReferencedDelegations`](crate::swarm::session_link::ReferencedDelegations) resolves, or a
//! link-control frame, which grants or withdraws such a reference. This module is the
//! imperative shell around that pure state: it reads the inbound clock, interprets the steps'
//! verdicts (deliver, drop, confirm, disclaim), and hands every resolved frame to
//! [`SwarmTransportCallback::verify_frame`] and then [`SwarmTransportCallback::gate_prepared_frame`],
//! where verification and admission are what they were before references existed.
//!
//! Link law: a reference resolves only on a link, and this callback's link is the handshake it
//! is bound to, with that handshake's peer, on that handshake's connection generation. Every
//! confirmation and disclaimer names that generation and leaves on it alone (see
//! [`SwarmTransport::send_link_control`](crate::swarm::transport::SwarmTransport::send_link_control)),
//! and only frames on the link teach the link's table. A frame from any other peer, or on a
//! callback bound to no handshake, has no link: it is judged self-contained, as it would be
//! outside any connection, a reference in it is refused as
//! [`Error::DelegationReferenceUnresolved`](crate::error::Error::DelegationReferenceUnresolved)
//! and charged, and nothing it carries reaches the table.
//!
//! Learning law: what a verified frame teaches the link, and the leases its references renewed,
//! are confirmed to the peer before the frame itself is gated.
//!
//! Charging law: a reference the link does not hold is a miss, and on a link a miss is this
//! end's clock or fault for an honest peer (the lease law of `swarm::session_link`): the frame
//! is dropped uncharged and the peer told `Unknown`, so it sends the session inline again.
//! Nothing is ever held, so the frames of one lane reach the logical stage in arrival order.

use std::str::FromStr;

use bytes::Bytes;
use rings_transport::core::callback::InboundCreditLease;
use rings_transport::core::pool::ChannelLane;

use super::FrameProvenance;
use super::InboundFrameLease;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use super::OnMessageRecursionDepthGuard;
use super::SwarmTransportCallback;
use super::TransportCallbackError;
use crate::dht::Did;
use crate::message::LinkControl;
use crate::message::LinkFrame;
use crate::message::WirePayload;
use crate::swarm::session_link::Confirmation;
use crate::swarm::session_link::Digests;
use crate::swarm::session_link::ResolvedFrame;
use crate::swarm::transport::PendingConnectionAttempt;

impl SwarmTransportCallback {
    /// Take one frame from the transport through the link stage.
    ///
    /// ```text
    ///   bytes ─decode─┬─ LinkControl ─▶ handle_link_control
    ///                 └─ Payload ─┬─ no link ─▶ self-contained ─▶ verify ─▶ gate
    ///                             └─ link ─resolve─┬─ resolved ─▶ verify ─▶ lease what it taught;
    ///                                              │              confirm leases and renewals ─▶ gate
    ///                                              └─ missed ───▶ dropped uncharged; Unknown(d)
    /// ```
    pub(super) async fn submit_inbound_message(
        &self,
        cid: &str,
        msg: Bytes,
        transport_credit: Option<InboundCreditLease>,
        carrier: Option<ChannelLane>,
    ) -> std::result::Result<(), TransportCallbackError> {
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        let _depth_guard = OnMessageRecursionDepthGuard::enter();
        // Test builds: the frame arrived; it stays in flight until it is dropped here or its
        // lease is released at the inbound actor's handoff.
        #[cfg(test)]
        let in_flight = self.processor.logical.transport.frames_for_test().arrive();

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
            transport_credit,
            carrier,
            #[cfg(test)]
            in_flight,
        };
        let Some(link) = self.bound_attempt(peer) else {
            return self.submit_off_link(peer, frame, lease).await;
        };
        let resolution = self
            .processor
            .session_link()
            .resolve(frame, self.processor.now_ms());
        match resolution {
            Ok(resolved) => self.admit_on_link(link, resolved, lease).await,
            Err(missing) => {
                tracing::debug!(
                    peer = %link.peer(),
                    missing = ?missing,
                    "dropping message; it references sessions this link does not hold"
                );
                // Dropping the lease is the effect: it releases the frame's transport credit.
                drop(lease);
                self.disclaim_delegations(link, missing).await;
                Ok(())
            }
        }
    }

    /// Verify one frame resolved on `link`, lease the sessions it carried inline, confirm them
    /// and the leases its references renewed, then gate the frame.
    async fn admit_on_link(
        &self,
        link: PendingConnectionAttempt,
        resolved: ResolvedFrame,
        lease: InboundFrameLease,
    ) -> std::result::Result<(), TransportCallbackError> {
        let peer = Some(link.peer());
        let ResolvedFrame {
            payload,
            encoding,
            renewals,
        } = resolved;
        let prepared = self.verify_frame(peer, payload, lease.bytes.len()).await?;
        let leased = self.processor.session_link().admit_verified(
            &prepared.payload,
            encoding,
            self.processor.now_ms(),
        )?;
        self.confirm_delegations(link, renewals.into_iter().chain(leased))
            .await;
        self.gate_prepared_frame(peer, prepared, lease, FrameProvenance::Arrived)
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

    /// Grant the peer the leases `confirm`, so it may reference those sessions.
    async fn confirm_delegations(
        &self,
        link: PendingConnectionAttempt,
        confirm: impl IntoIterator<Item = Confirmation>,
    ) {
        for confirmation in confirm {
            let _dispatched = self.emit_link_control(link, confirmation.control()).await;
        }
    }

    /// Tell the peer that this link does not hold the sessions behind `missing`, so it sends
    /// them inline again.
    async fn disclaim_delegations(&self, link: PendingConnectionAttempt, missing: Digests) {
        for digest in missing {
            let _dispatched = self
                .emit_link_control(link, LinkControl::Unknown(digest))
                .await;
        }
    }

    /// Emit one control frame on `link`. Post: whether it was dispatched; a refusal is logged,
    /// since the next frame that misses or teaches the same session repeats the disclaimer or
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
    ///   Known(d, until) ─▶ lease d until `until` in the announced table of this generation
    ///   Unknown(d)      ─▶ d travels inline again on this generation
    /// ```
    async fn handle_link_control(&self, peer: Option<Did>, control: LinkControl) {
        let Some(link) = self.bound_attempt(peer) else {
            tracing::debug!(peer = ?peer, "ignoring link control outside a bound connection");
            return;
        };
        let transport = &self.processor.logical.transport;
        match control {
            LinkControl::Known { digest, until_ms } => {
                transport.acknowledge_session(link, digest, until_ms)
            }
            LinkControl::Unknown(digest) => transport.disclaimed_session(link, digest),
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
}
