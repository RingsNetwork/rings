use std::collections::BTreeMap;
use std::ops::Bound::Excluded;
use std::ops::Bound::Unbounded;
use std::sync::Arc;
use std::sync::Weak;

use crate::dht::Did;
use crate::utils::FairWaitQueue;

type PeerKey = Option<Did>;

pub(super) struct WakeTarget {
    pub(super) peer: PeerKey,
    pub(super) queue: Arc<FairWaitQueue>,
    pub(super) remaining: usize,
}

#[derive(Clone, Copy)]
enum WakeSelection {
    Any,
    OtherThan(PeerKey),
}

impl WakeSelection {
    fn includes(self, peer: PeerKey) -> bool {
        match self {
            Self::Any => true,
            Self::OtherThan(excluded) => peer != excluded,
        }
    }
}

#[derive(Default)]
pub(super) struct InboundWaitQueues {
    queues: BTreeMap<PeerKey, Weak<FairWaitQueue>>,
    last_woken: Option<PeerKey>,
}

impl InboundWaitQueues {
    pub(super) fn queue_for_peer(&mut self, peer: PeerKey) -> Arc<FairWaitQueue> {
        if let Some(queue) = self.existing_queue(peer) {
            return queue;
        }
        self.prune_expired();
        let queue = Arc::new(FairWaitQueue::new());
        self.queues.insert(peer, Arc::downgrade(&queue));
        queue
    }

    fn prune_expired(&mut self) {
        self.queues.retain(|_, queue| queue.strong_count() > 0);
    }

    pub(super) fn existing_queue(&mut self, peer: PeerKey) -> Option<Arc<FairWaitQueue>> {
        match self.queues.get(&peer).and_then(Weak::upgrade) {
            Some(queue) => Some(queue),
            None => {
                self.queues.remove(&peer);
                None
            }
        }
    }

    fn next_key_after(&self, key: Option<PeerKey>) -> Option<PeerKey> {
        key.and_then(|key| {
            self.queues
                .range((Excluded(key), Unbounded))
                .next()
                .map(|(peer, _)| *peer)
        })
        .or_else(|| self.queues.keys().next().copied())
    }

    fn next_live_queue(
        &mut self,
        selection: WakeSelection,
    ) -> Option<(PeerKey, Arc<FairWaitQueue>)> {
        let mut cursor = self.last_woken;
        let mut remaining = self.queues.len();
        while remaining > 0 {
            let peer = self.next_key_after(cursor)?;
            cursor = Some(peer);
            remaining = remaining.saturating_sub(1);
            if !selection.includes(peer) {
                continue;
            }
            match self.existing_queue(peer) {
                Some(queue) => {
                    self.last_woken = Some(peer);
                    return Some((peer, queue));
                }
                None => continue,
            }
        }
        None
    }

    pub(super) fn start_wake_round(&mut self) -> Option<WakeTarget> {
        let remaining = self.queues.len().saturating_sub(1);
        self.next_live_queue(WakeSelection::Any)
            .map(|(peer, queue)| WakeTarget {
                peer,
                queue,
                remaining,
            })
    }

    pub(super) fn continue_wake_round(
        &mut self,
        current_peer: PeerKey,
        remaining: usize,
    ) -> Option<WakeTarget> {
        if remaining == 0 {
            return None;
        }
        self.next_live_queue(WakeSelection::OtherThan(current_peer))
            .map(|(peer, queue)| WakeTarget {
                peer,
                queue,
                remaining: remaining.saturating_sub(1),
            })
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.queues.len()
    }
}
