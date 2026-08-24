use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Weak;

use crate::dht::Did;
use crate::utils::CoordinatedFairWaitQueue;
use crate::utils::FairHandoff;
use crate::utils::FairWakeRound;

type PeerKey = Option<Did>;

pub(super) struct WakeTarget {
    pub(super) peer: PeerKey,
    pub(super) queue: Arc<CoordinatedFairWaitQueue>,
    pub(super) round: FairWakeRound,
}

struct WakeRound {
    id: FairWakeRound,
    current: Option<PeerKey>,
    candidates: VecDeque<PeerKey>,
}

#[derive(Default)]
pub(super) struct InboundWaitQueues {
    queues: BTreeMap<PeerKey, Weak<CoordinatedFairWaitQueue>>,
    last_woken: Option<PeerKey>,
    // Invariant: at most one round owns a fixed, duplicate-free peer snapshot.
    // Releases during that round coalesce into one repeat scan through pending_wake.
    active_round: Option<WakeRound>,
    pending_wake: bool,
}

impl InboundWaitQueues {
    pub(super) fn queue_for_peer(&mut self, peer: PeerKey) -> Arc<CoordinatedFairWaitQueue> {
        if let Some(queue) = self.existing_queue(peer) {
            return queue;
        }
        self.prune_expired();
        let queue = Arc::new(CoordinatedFairWaitQueue::coordinated());
        self.queues.insert(peer, Arc::downgrade(&queue));
        queue
    }

    fn prune_expired(&mut self) {
        self.queues.retain(|_, queue| queue.strong_count() > 0);
    }

    pub(super) fn existing_queue(
        &mut self,
        peer: PeerKey,
    ) -> Option<Arc<CoordinatedFairWaitQueue>> {
        match self.queues.get(&peer).and_then(Weak::upgrade) {
            Some(queue) => Some(queue),
            None => {
                self.queues.remove(&peer);
                None
            }
        }
    }

    fn round_candidates(&self) -> VecDeque<PeerKey> {
        let mut candidates = self.queues.keys().copied().collect::<Vec<_>>();
        if let Some(last_woken) = self.last_woken {
            let split = candidates.partition_point(|peer| *peer <= last_woken);
            if split < candidates.len() {
                candidates.rotate_left(split);
            }
        }
        candidates.into()
    }

    fn begin_round(&mut self) -> Option<WakeTarget> {
        self.prune_expired();
        let candidates = self.round_candidates();
        if candidates.is_empty() {
            return None;
        }
        let round = FairWakeRound::new();
        self.active_round = Some(WakeRound {
            id: round,
            current: None,
            candidates,
        });
        let target = self.next_target();
        if target.is_none() {
            self.active_round = None;
        }
        target
    }

    fn next_target(&mut self) -> Option<WakeTarget> {
        loop {
            let (round, peer) = {
                let active = self.active_round.as_mut()?;
                (active.id.clone(), active.candidates.pop_front()?)
            };
            let Some(queue) = self.existing_queue(peer) else {
                continue;
            };
            if let Some(active) = self.active_round.as_mut() {
                active.current = Some(peer);
            }
            self.last_woken = Some(peer);
            return Some(WakeTarget { peer, queue, round });
        }
    }

    fn matches_active(&self, peer: PeerKey, round: &FairWakeRound) -> bool {
        self.active_round
            .as_ref()
            .is_some_and(|active| active.id.same(round) && active.current == Some(peer))
    }

    fn finish_exhausted_round(&mut self) -> Option<WakeTarget> {
        self.active_round = None;
        if std::mem::take(&mut self.pending_wake) {
            self.begin_round()
        } else {
            None
        }
    }

    /// Start one serialized scan, or record that the active scan must be repeated.
    pub(super) fn request_wake_round(&mut self) -> Option<WakeTarget> {
        if self.active_round.is_some() {
            self.pending_wake = true;
            return None;
        }
        self.begin_round()
    }

    /// Resolve the active round's unique handoff token.
    pub(super) fn handle_handoff(
        &mut self,
        peer: PeerKey,
        handoff: FairHandoff,
    ) -> Option<WakeTarget> {
        let (round, made_progress) = match handoff {
            FairHandoff::HeadAdvanced => return self.request_wake_round(),
            FairHandoff::Continue(round) => (round, false),
            FairHandoff::Progress(round) => (round, true),
        };
        if !self.matches_active(peer, &round) {
            return None;
        }
        if made_progress {
            self.active_round = None;
            self.pending_wake = false;
            return self.begin_round();
        }
        if let Some(active) = self.active_round.as_mut() {
            active.current = None;
        }
        self.next_target().or_else(|| self.finish_exhausted_round())
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.queues.len()
    }
}
