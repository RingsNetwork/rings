//! Outbound transfers whose count their sender's own law bounds, so they take none of the shared
//! outbound budgets: other traffic toward a peer that withholds credit can never refuse them.
//!
//! ```text
//!   Probe  ── at most one outstanding per peer generation (`transport::liveness`)
//!   Answer ── at most one in flight per next hop, held by its permit (this module), and only
//!             for an answer sent straight to the prober it answers (`egress::ProbeAnswer`)
//! ```
//!
//! Law: every sender-bounded transfer is bounded per peer by one of the two rules above, so
//! their total is bounded by twice the admitted peers. A probe answer that finds its peer's slot
//! taken falls back to the shared budget, as any protocol send. This module is the one
//! statement of the law; the egress and the liveness clock refer to it.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use crate::dht::Did;

/// Which law bounds a sender-bounded transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SenderBound {
    /// A liveness probe of the peer's connection `generation`: the probe clock admits one
    /// outstanding per peer generation, and the probe rides that generation's connection only.
    Probe { generation: u64 },
    /// A probe's answer to its next hop, the prober: one in flight per next hop.
    Answer,
}

/// The bookkeeping of sender-bounded transfers: the answer slots in use, and (test builds) how
/// many such transfers are in flight, so quiescence probes see them.
#[derive(Default)]
pub(in crate::swarm::transport) struct SenderBounded {
    answers: Arc<Mutex<BTreeSet<Did>>>,
    #[cfg(test)]
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
}

/// One sender-bounded transfer, held by its permit for the transfer's life.
pub(in crate::swarm::transport) struct SenderBoundedTransfer {
    /// The answer slot it holds, released on drop.
    answer: Option<(Arc<Mutex<BTreeSet<Did>>>, Did)>,
    #[cfg(test)]
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
}

/// The answer slots; every critical section leaves them consistent.
fn slots(answers: &Mutex<BTreeSet<Did>>) -> MutexGuard<'_, BTreeSet<Did>> {
    answers.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SenderBounded {
    /// Admit one transfer toward `peer` under `bound`: always for a probe, for an answer only
    /// while `peer` holds no other answer.
    pub(in crate::swarm::transport) fn admit(
        &self,
        bound: SenderBound,
        peer: Did,
    ) -> Option<SenderBoundedTransfer> {
        let answer = match bound {
            SenderBound::Probe { .. } => None,
            SenderBound::Answer => {
                if !slots(&self.answers).insert(peer) {
                    return None;
                }
                Some((Arc::clone(&self.answers), peer))
            }
        };
        #[cfg(test)]
        self.in_flight
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Some(SenderBoundedTransfer {
            answer,
            #[cfg(test)]
            in_flight: Arc::clone(&self.in_flight),
        })
    }

    /// Test builds: the sender-bounded transfers in flight.
    #[cfg(test)]
    pub(in crate::swarm::transport) fn in_flight_for_test(&self) -> usize {
        self.in_flight.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Drop for SenderBoundedTransfer {
    fn drop(&mut self) {
        if let Some((answers, peer)) = self.answer.take() {
            slots(&answers).remove(&peer);
        }
        #[cfg(test)]
        {
            self.in_flight
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            // A released transfer is a state change that quiescence probes read.
            crate::tests::activity::record_activity();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SenderBound;
    use super::SenderBounded;
    use crate::ecc::SecretKey;

    /// One answer in flight per next hop; probes are bounded elsewhere and always admitted.
    #[test]
    fn test_one_answer_in_flight_per_next_hop() {
        let bounded = SenderBounded::default();
        let peer = SecretKey::random().address().into();
        let first = bounded.admit(SenderBound::Answer, peer);
        assert!(first.is_some());
        assert!(bounded.admit(SenderBound::Answer, peer).is_none());
        assert!(bounded
            .admit(SenderBound::Probe { generation: 0 }, peer)
            .is_some());
        drop(first);
        assert!(bounded.admit(SenderBound::Answer, peer).is_some());
    }
}
