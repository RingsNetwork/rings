//! The link stage of one inbound connection: the first thing a frame from the transport meets.
//!
//! A frame is a payload, whose session slots may be references that this connection's
//! [`ReferencedSessions`](crate::swarm::session_link::ReferencedSessions) resolves, or a
//! link-control frame, which asks about or answers such a reference. This module is the
//! imperative shell around that pure state: it reads the clock, interprets the effects the
//! steps return (deliver, drop, ask the peer), and hands every resolved frame to
//! [`InnerSwarmCallback::admit_resolved_frame`], where verification and admission are what they
//! were before references existed.

use std::str::FromStr;

use bytes::Bytes;
use rings_transport::core::callback::InboundFrameCapacityLease;

use super::InnerSwarmCallback;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use super::OnMessageRecursionDepthGuard;
use super::TransportCallbackError;
use super::UnresolvedInboundFrame;
use crate::dht::Did;
use crate::measure::Authentication;
use crate::message::LinkFrame;
use crate::message::SessionControl;
use crate::session::SessionDigest;
use crate::swarm::session_link::FrameArrival;
use crate::swarm::session_link::FrameRelease;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::utils::get_epoch_ms;

impl InnerSwarmCallback {
    /// Take one frame from the transport through the link stage.
    ///
    /// ```text
    ///   bytes ─decode─┬─ SessionControl ──▶ handle_link_control
    ///                 └─ Payload ─arrive─┬─ Resolved ─▶ admit_resolved_frame ─▶ confirm what it taught,
    ///                                    │                                     release what awaited it
    ///                                    ├─ Held ─────▶ ask the peer for what the frame misses
    ///                                    └─ Overflow ─▶ dropped; ask again for everything awaited
    /// ```
    pub(super) async fn submit_inbound_message(
        &self,
        cid: &str,
        msg: Bytes,
        transport_capacity: Option<InboundFrameCapacityLease>,
    ) -> Result<(), TransportCallbackError> {
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
        let carrier = UnresolvedInboundFrame {
            bytes: msg,
            transport_capacity,
        };
        let arrival = self
            .processor
            .session_link()
            .arrive(frame, carrier, get_epoch_ms());
        match arrival {
            Ok(FrameArrival::Resolved(resolved)) => {
                let learned = self.admit_resolved_frame(peer, resolved).await?;
                self.learned_sessions(peer, learned).await;
                Ok(())
            }
            Ok(FrameArrival::Held { request }) => {
                self.request_sessions(peer, request).await;
                Ok(())
            }
            Ok(FrameArrival::Overflow { request }) => {
                tracing::debug!(
                    peer = ?peer,
                    "dropping message; the hold for unresolved session references is full"
                );
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
        let authentication = peer.map_or(Authentication::Unauthenticated, |peer| {
            self.processor.peer_authentication(peer)
        });
        self.processor
            .record_receive_failure(peer, authentication)
            .await;
    }

    /// Deliver every held frame that can leave now: one that resolves, or one whose proof
    /// lapsed, which is dropped.
    ///
    /// A failure on one released frame is logged and does not stop the release: the frame was
    /// accepted from the transport when it arrived, and the others are independent of it.
    async fn release_held_frames(&self, peer: Option<Did>) {
        loop {
            let release = self.processor.session_link().release_next(get_epoch_ms());
            match release {
                Ok(Some(FrameRelease::Resolved(resolved))) => {
                    // The loop re-scans the hold, so what this frame taught is applied to the
                    // frames still held without recursing.
                    let learned = self.admit_resolved_frame(peer, resolved).await;
                    let learned = learned.unwrap_or_else(|error| {
                        tracing::warn!(
                            peer = ?peer,
                            error = ?error,
                            "failed to deliver a message held for a session reference"
                        );
                        Vec::new()
                    });
                    self.confirm_sessions(peer, learned).await;
                }
                Ok(Some(FrameRelease::Lapsed(_))) => {
                    tracing::debug!(
                        peer = ?peer,
                        "dropping a message whose proof lapsed while its session was unresolved"
                    );
                }
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(peer = ?peer, error = ?error, "releasing a held message failed");
                    return;
                }
            }
        }
    }

    /// The link learned `learned` from a frame that arrived: confirm them to `peer`, and let
    /// the held frames that awaited one of them leave.
    async fn learned_sessions(&self, peer: Option<Did>, learned: Vec<SessionDigest>) {
        let releases = !learned.is_empty();
        self.confirm_sessions(peer, learned).await;
        if releases {
            self.release_held_frames(peer).await;
        }
    }

    /// Tell `peer` that the sessions behind `confirm` verified here, so it may reference them.
    async fn confirm_sessions(&self, peer: Option<Did>, confirm: Vec<SessionDigest>) {
        let Some(peer) = peer else {
            return;
        };
        for digest in confirm {
            self.send_session_control(peer, SessionControl::Known(digest))
                .await;
        }
    }

    /// Ask `peer` for the sessions behind `request`.
    async fn request_sessions(&self, peer: Option<Did>, request: Vec<SessionDigest>) {
        let Some(peer) = peer else {
            tracing::warn!("cannot ask an unparsable peer for a session");
            return;
        };
        for digest in request {
            self.send_session_control(peer, SessionControl::Request(digest))
                .await;
        }
    }

    /// Send one control frame to `peer`; a failure is logged, since the next arrival repeats
    /// the question or the confirmation.
    async fn send_session_control(&self, peer: Did, control: SessionControl) {
        if let Err(error) = self
            .processor
            .logical
            .transport
            .send_link_control(peer, &control)
            .await
        {
            tracing::debug!(peer = %peer, error = ?error, control = ?control, "failed to send session control");
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
    async fn handle_link_control(&self, peer: Option<Did>, control: SessionControl) {
        let Some(peer) = peer else {
            tracing::warn!("ignoring link control from an unparsable peer");
            return;
        };
        let unavailable = match control {
            SessionControl::Known(digest) => {
                self.acknowledge_session(peer, digest);
                return;
            }
            SessionControl::Request(digest) => {
                self.answer_session_request(peer, digest).await;
                return;
            }
            SessionControl::Announce(session) => self
                .processor
                .session_link()
                .announce(session, get_epoch_ms()),
            SessionControl::Unknown(digest) => Ok(self
                .processor
                .session_link()
                .unknown(digest, get_epoch_ms())),
        };
        match unavailable {
            Ok(unavailable) => {
                for _frame in unavailable {
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
        self.release_held_frames(Some(peer)).await;
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
