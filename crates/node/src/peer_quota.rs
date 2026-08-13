use std::collections::HashMap;

use rings_core::dht::Did;

use crate::error::OnionQueueAdmissionReason;

/// Pure global/per-peer cardinality bound shared by queue and session reducers.
///
/// Invariant: `total = sum(per_peer.values())`, zero peer entries are absent,
/// `total <= max_total`, and each peer count is at most `max_per_peer`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PeerQuota {
    total: usize,
    per_peer: HashMap<Did, usize>,
    max_total: usize,
    max_per_peer: usize,
}

impl PeerQuota {
    pub(crate) fn new(max_total: usize, max_per_peer: usize) -> Self {
        Self {
            total: 0,
            per_peer: HashMap::new(),
            max_total,
            max_per_peer,
        }
    }

    pub(crate) const fn total(&self) -> usize {
        self.total
    }

    pub(crate) fn peer_total(&self, peer: Did) -> usize {
        self.per_peer.get(&peer).copied().unwrap_or_default()
    }

    /// Prove that one successor reservation is within both bounds without mutating state.
    pub(crate) fn can_reserve(&self, peer: Did) -> Result<(), OnionQueueAdmissionReason> {
        self.successor(peer).map(|_| ())
    }

    fn successor(&self, peer: Did) -> Result<(usize, usize), OnionQueueAdmissionReason> {
        let next_total = self
            .total
            .checked_add(1)
            .ok_or(OnionQueueAdmissionReason::CounterOverflow)?;
        let next_peer = self
            .peer_total(peer)
            .checked_add(1)
            .ok_or(OnionQueueAdmissionReason::CounterOverflow)?;
        if next_total > self.max_total {
            return Err(OnionQueueAdmissionReason::GlobalFull);
        }
        if next_peer > self.max_per_peer {
            return Err(OnionQueueAdmissionReason::PeerFull);
        }
        Ok((next_total, next_peer))
    }

    /// Reserve one unit atomically after the immutable successor proof succeeds.
    pub(crate) fn reserve(&mut self, peer: Did) -> Result<(), OnionQueueAdmissionReason> {
        let (next_total, next_peer) = self.successor(peer)?;
        self.total = next_total;
        self.per_peer.insert(peer, next_peer);
        Ok(())
    }

    /// Release one existing unit; an inconsistent predecessor leaves state unchanged.
    pub(crate) fn release(&mut self, peer: Did) -> bool {
        let Some(peer_total) = self.per_peer.get(&peer).copied() else {
            return false;
        };
        let Some(next_total) = self.total.checked_sub(1) else {
            return false;
        };
        let Some(next_peer) = peer_total.checked_sub(1) else {
            return false;
        };
        self.total = next_total;
        if next_peer == 0 {
            self.per_peer.remove(&peer);
        } else {
            self.per_peer.insert(peer, next_peer);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_and_release_preserve_global_and_peer_projection() {
        let mut quota = PeerQuota::new(2, 1);
        let first = Did::from(1_u32);
        let second = Did::from(2_u32);

        assert_eq!(quota.reserve(first), Ok(()));
        assert_eq!(
            quota.reserve(first),
            Err(OnionQueueAdmissionReason::PeerFull)
        );
        assert_eq!(quota.total(), 1);
        assert_eq!(quota.peer_total(first), 1);
        assert_eq!(quota.reserve(second), Ok(()));
        assert_eq!(
            quota.reserve(Did::from(3_u32)),
            Err(OnionQueueAdmissionReason::GlobalFull)
        );
        assert!(quota.release(first));
        assert!(!quota.release(first));
        assert_eq!(quota.total(), 1);
        assert_eq!(quota.peer_total(second), 1);
    }
}
