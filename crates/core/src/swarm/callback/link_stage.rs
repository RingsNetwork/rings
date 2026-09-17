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
use crate::utils::get_epoch_ms;

impl InnerSwarmCallback {
    /// Take one frame from the transport through the link stage.
    ///
    /// ```text
    ///   bytes ─decode─┬─ SessionControl ──▶ handle_link_control
    ///                 └─ Payload ─arrive─┬─ Resolved ─▶ admit_resolved_frame
    ///                                    ├─ Held ─────▶ drain (asks the peer for the head's misses)
    ///                                    └─ Overflow ─▶ dropped, then drain (asks again)
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
            Ok(FrameArrival::Resolved(resolved)) => self.admit_resolved_frame(peer, resolved).await,
            Ok(FrameArrival::Held) => {
                self.drain_session_hold(peer).await;
                Ok(())
            }
            Ok(FrameArrival::Overflow(_)) => {
                tracing::debug!(
                    peer = ?peer,
                    "dropping message; the hold for unresolved session references is full"
                );
                // The hold is full because its head is still unanswered: ask again, so a lost
                // question cannot wedge the link while frames keep arriving.
                self.drain_session_hold(peer).await;
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

    /// Release held frames in arrival order until the hold is empty or its head misses a
    /// session, then ask the peer for what the head misses.
    ///
    /// A failure on one released frame is logged and does not stop the drain: the frame was
    /// accepted from the transport when it arrived, and the frames behind it are independent of
    /// it. A question that cannot be sent is not retried here; the next arrival or answer
    /// drains again.
    async fn drain_session_hold(&self, peer: Option<Did>) {
        if !self.processor.session_link().begin_drain() {
            return;
        }
        loop {
            let release = self.processor.session_link().release_next(get_epoch_ms());
            match release {
                Ok(FrameRelease::Resolved(resolved)) => {
                    if let Err(error) = self.admit_resolved_frame(peer, resolved).await {
                        tracing::warn!(
                            peer = ?peer,
                            error = ?error,
                            "failed to deliver a message held for a session reference"
                        );
                    }
                }
                Ok(FrameRelease::Lapsed(_)) => {
                    tracing::debug!(
                        peer = ?peer,
                        "dropping a message whose proof lapsed while its session was unresolved"
                    );
                }
                Ok(FrameRelease::Blocked(request)) => {
                    self.request_sessions(peer, request).await;
                    return;
                }
                Ok(FrameRelease::Drained) => return,
                Err(error) => {
                    tracing::warn!(peer = ?peer, error = ?error, "session hold drain failed");
                    return;
                }
            }
        }
    }

    /// Ask `peer` for the sessions behind `request`.
    async fn request_sessions(&self, peer: Option<Did>, request: Vec<SessionDigest>) {
        let Some(peer) = peer else {
            tracing::warn!("cannot ask an unparsable peer for a session");
            return;
        };
        for digest in request {
            let question = SessionControl::Request(digest);
            if let Err(error) = self
                .processor
                .logical
                .transport
                .send_link_control(peer, &question)
                .await
            {
                tracing::debug!(peer = %peer, error = ?error, "failed to request a session");
            }
        }
    }

    /// Act on one link-control frame from `peer`.
    ///
    /// ```text
    ///   Request(d)  ─▶ answer from the announced table of this connection generation
    ///   Announce(s) ─▶ announce ─▶ fail refused frames ─▶ drain
    ///   Unknown(d)  ─▶ unknown  ─▶ fail awaiting frames ─▶ drain
    /// ```
    async fn handle_link_control(&self, peer: Option<Did>, control: SessionControl) {
        let Some(peer) = peer else {
            tracing::warn!("ignoring link control from an unparsable peer");
            return;
        };
        let unavailable = match control {
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
        self.drain_session_hold(Some(peer)).await;
    }

    /// Answer `peer`'s question about `digest` from what this connection generation announced.
    /// A callback bound to no handshake, or to another peer's, has announced nothing to `peer`.
    async fn answer_session_request(&self, peer: Did, digest: SessionDigest) {
        let Some(attempt) = self
            .pending_attempt()
            .filter(|attempt| attempt.peer() == peer)
        else {
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
