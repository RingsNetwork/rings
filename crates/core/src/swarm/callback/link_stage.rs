//! The link stage of one inbound connection: the first thing a frame from the transport meets.
//!
//! A frame is a payload, whose delegation slots may be references that this connection's
//! [`DelegationLessor`](crate::swarm::session_link::DelegationLessor) resolves, or a
//! link-control frame, which grants or withdraws such a reference. This module is the
//! imperative shell around that pure state: it reads the inbound clock, interprets the steps'
//! verdicts (deliver, drop, confirm, disclaim), and hands every resolved frame to
//! [`BoundCallback::verify_frame`] and then [`BoundCallback::gate_prepared_frame`],
//! where verification and admission are what they were before references existed.
//!
//! Link law: a reference resolves only on a link, and this callback's link is the handshake it
//! is bound to, with that handshake's peer, on that handshake's connection generation. Every
//! lease and disclaimer is judged on that generation and leaves on its connection alone (see
//! [`SwarmTransport::send_link_control`](crate::swarm::transport::SwarmTransport::send_link_control)),
//! and only frames on the link teach the link's table. A frame from any other peer, or on a
//! callback bound to no handshake, has no link: it is judged self-contained, as it would be
//! outside any connection, a reference in it is refused as
//! [`Error::DelegationReferenceUnresolved`](crate::error::Error::DelegationReferenceUnresolved)
//! and charged, and nothing it carries reaches the table.
//!
//! Learning law: only a verified frame teaches the link, and what it taught (the leases its
//! inline sessions earned and its references renewed) is confirmed to the peer before the
//! frame itself is gated.
//!
//! Charging law: a reference the link does not hold is a miss, and on an honest link a miss
//! means transits beyond `Δ + G` under a full table, or a fault at this end (the laws of
//! `swarm::session_link`): the frame is dropped uncharged and the peer told `Unknown`, so it
//! sends the session inline again. Nothing is ever held, so the frames of one lane reach the
//! logical stage in arrival order.

use std::str::FromStr;

use bytes::Bytes;
use rings_transport::core::callback::InboundCreditLease;
use rings_transport::core::pool::ChannelLane;

use super::BoundCallback;
use super::InboundFrameLease;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use super::OnMessageRecursionDepthGuard;
use super::TransportCallbackError;
use crate::dht::Did;
use crate::message::LinkControl;
use crate::message::LinkFrame;
use crate::message::WirePayload;
use crate::swarm::session_link::Resolution;
use crate::swarm::session_link::ResolvedFrame;
use crate::swarm::transport::PendingConnectionAttempt;

impl BoundCallback {
    /// Take one frame from the transport through the link stage.
    ///
    /// ```text
    ///   bytes ─decode─┬─ LinkControl ─▶ handle_link_control
    ///                 └─ Payload ─┬─ no link ─▶ self-contained ─▶ verify ─▶ gate
    ///                             └─ link ─resolve─┬─ resolved ─▶ verify ─▶ lease and renew;
    ///                                              │              confirm them ─▶ gate
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
                self.handle_link_control(peer, control);
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
        let resolution = self.processor.session_link().resolve(frame);
        match resolution {
            Resolution::Resolved(resolved) => self.admit_on_link(link, *resolved, lease).await,
            Resolution::Missed(missing) => {
                tracing::debug!(
                    peer = %link.peer(),
                    missing = ?missing,
                    "dropping message; it references sessions this link does not hold"
                );
                // Dropping the lease is the effect: it releases the frame's transport credit.
                drop(lease);
                self.emit_link_controls(link, missing.into_iter().map(LinkControl::Unknown))
                    .await;
                Ok(())
            }
        }
    }

    /// Verify one frame resolved on `link`, lease the sessions it carried inline and renew the
    /// leases its references found short, confirm them, then gate the frame.
    async fn admit_on_link(
        &self,
        link: PendingConnectionAttempt,
        resolved: ResolvedFrame,
        lease: InboundFrameLease,
    ) -> std::result::Result<(), TransportCallbackError> {
        let peer = Some(link.peer());
        let ResolvedFrame { payload, encoding } = resolved;
        let Some(prepared) = self.verify_frame(peer, payload, lease.bytes.len()).await? else {
            return Ok(());
        };
        let confirm = self.processor.session_link().admit_verified(
            &prepared.payload,
            encoding,
            self.processor.now_ms(),
        )?;
        self.emit_link_controls(link, confirm.into_iter().map(LinkControl::from))
            .await;
        self.gate_prepared_frame(peer, prepared, lease).await
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
        let Some(prepared) = self.verify_frame(peer, payload, lease.bytes.len()).await? else {
            return Ok(());
        };
        self.gate_prepared_frame(peer, prepared, lease).await
    }

    /// Send `controls` on `link`, one frame each. A refusal is logged and nothing more: the
    /// next frame that misses or teaches the same session repeats the disclaimer or the
    /// confirmation.
    async fn emit_link_controls(
        &self,
        link: PendingConnectionAttempt,
        controls: impl IntoIterator<Item = LinkControl>,
    ) {
        let transport = &self.processor.logical.transport;
        for control in controls {
            if let Err(error) = transport.send_link_control(link, &control).await {
                tracing::debug!(
                    peer = %link.peer(),
                    error = ?error,
                    control = ?control,
                    "link control not sent"
                );
            }
        }
    }

    /// Act on one link-control frame from `peer`: the lessee table of this generation learns
    /// it (see `DelegationLessee::observe`). A frame from a peer this callback is not bound to
    /// is on no link and changes nothing.
    fn handle_link_control(&self, peer: Option<Did>, control: LinkControl) {
        let Some(link) = self.bound_attempt(peer) else {
            tracing::debug!(peer = ?peer, "ignoring link control outside a bound connection");
            return;
        };
        self.processor
            .logical
            .transport
            .observe_link_control(link, &control);
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
