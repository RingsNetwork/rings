//! The sessions this end has sent one peer inline: the table the outbound worker encodes frames
//! against, shared with the inbound side that marks the peer's confirmations and answers its
//! questions about it.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use bytes::Bytes;

use super::transfer::OutboundFrame;
use super::OutboundSchedulers;
use crate::dht::Did;
use crate::error::Result;
use crate::message::SessionControl;
use crate::message::WirePayload;
use crate::session::SessionDigest;
use crate::swarm::session_link::AnnouncedSessions;

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

    /// The bytes of `frame` on the link of `generation` at `now_ms`.
    ///
    /// A control frame is already bytes. A payload's session slots are decided by the table:
    /// confirmed sessions travel by digest, every other session inline.
    pub(super) fn encode(
        &self,
        generation: u64,
        frame: OutboundFrame,
        now_ms: u128,
    ) -> Result<Bytes> {
        match frame {
            OutboundFrame::Control(bytes) => Ok(bytes),
            OutboundFrame::Payload(payload) => {
                let payload = payload.as_ref();
                let sessions = self.lock().encode(generation, payload, now_ms)?;
                #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
                super::test_trace::record_encoded_frame(&sessions);
                WirePayload::view(payload, sessions).to_wire()
            }
        }
    }
}

impl OutboundSchedulers {
    /// The peer confirmed `digest` on the link of `generation`: later frames to it reference
    /// the session. A peer this end has no scheduler for was sent nothing.
    pub(in crate::swarm::transport) fn acknowledge_session(
        &self,
        peer: Did,
        generation: u64,
        digest: SessionDigest,
    ) {
        if let Some(handle) = self.lock_registry().peers.get(&peer).cloned() {
            handle
                .state
                .announced
                .lock()
                .acknowledge(generation, digest);
        }
    }

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
