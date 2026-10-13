//! The sessions this end has sent one peer inline: the lessee table the outbound worker encodes
//! frames against, shared with the inbound side that records the leases the peer grants and the
//! sessions it disclaims.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use bytes::Bytes;

use super::OutboundSchedulers;
use crate::dht::Did;
use crate::error::Result;
use crate::message::LinkControl;
use crate::message::MessagePayload;
use crate::message::WirePayload;
use crate::swarm::session_link::DelegationLessee;

/// The one handle on a peer's [`DelegationLessee`]: every access is one pure step under the
/// lock.
///
/// Lock law: the lock is never held across a suspension point, and a poisoned lock still guards
/// a well-formed table, since no step panics between two writes. Clone law: clones name the
/// same table.
#[derive(Clone)]
pub(super) struct SharedDelegationLessee(Arc<Mutex<DelegationLessee>>);

impl SharedDelegationLessee {
    /// The table of a peer nothing has been sent to.
    pub(super) fn new() -> Self {
        Self(Arc::new(Mutex::new(DelegationLessee::new())))
    }

    /// Whether `other` names this very table.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(super) fn is_same_table(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// The table, for one step.
    fn lock(&self) -> MutexGuard<'_, DelegationLessee> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The bytes of `payload` on the link of `generation` at `now_ms`: its delegation slots are
    /// decided by the table, sessions leased with more than `Δ` left by digest and every other
    /// session inline.
    pub(super) fn encode(
        &self,
        generation: u64,
        payload: &MessagePayload,
        now_ms: u128,
    ) -> Result<Bytes> {
        let sessions = self.lock().encode(generation, payload, now_ms)?;
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        super::test_trace::record_encoded_frame(
            (
                crate::message::MessageVerificationExt::signer(payload),
                payload.relay.next_hop,
            ),
            &sessions,
        );
        WirePayload::view(payload, sessions).to_wire()
    }
}

impl OutboundSchedulers {
    /// Learn what `peer` said at `now_ms` about the link of `generation`; see
    /// [`DelegationLessee::observe`]. A peer this end has no link state for was sent nothing.
    pub(in crate::swarm::transport) fn observe_link_control(
        &self,
        peer: Did,
        generation: u64,
        control: &LinkControl,
        now_ms: u128,
    ) {
        if let Some(link) = self.link_of(peer) {
            link.lessee.lock().observe(generation, control, now_ms);
        }
    }
}
