use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use crate::dht::Did;
use crate::utils::CoordinatedFairWaitQueue;
use crate::utils::FairCapacityDemand;
use crate::utils::FairHandoff;
use crate::utils::FairWakeArm;
use crate::utils::FairWakeRound;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PeerKey {
    Anonymous,
    Identified(Did),
}

impl PeerKey {
    const fn into_peer(self) -> Option<Did> {
        match self {
            Self::Anonymous => None,
            Self::Identified(peer) => Some(peer),
        }
    }
}

impl From<Option<Did>> for PeerKey {
    fn from(peer: Option<Did>) -> Self {
        match peer {
            Some(peer) => Self::Identified(peer),
            None => Self::Anonymous,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum AfterProgress {
    /// No FIFO head fits the remaining aggregate message and byte capacity.
    Stop,
    /// The cheapest FIFO head may fit the remaining aggregate capacity.
    Scan,
}

impl AfterProgress {
    pub(super) const fn from_capacity(can_scan: bool) -> Self {
        if can_scan {
            Self::Scan
        } else {
            Self::Stop
        }
    }
}

pub(super) struct WakeTarget {
    pub(super) peer: Option<Did>,
    pub(super) queue: Arc<CoordinatedFairWaitQueue>,
    pub(super) round: FairWakeRound,
}

pub(super) fn wake_waiter(waiters: &Mutex<InboundWaitQueues>, mut target: Option<WakeTarget>) {
    while let Some(next) = target {
        match next.queue.wake_front_with_handoff(next.round.clone()) {
            FairWakeArm::Armed => return,
            FairWakeArm::Empty => {}
            FairWakeArm::AlreadyArmed => {
                tracing::warn!(
                    peer = ?next.peer,
                    round = ?next.round,
                    "coordinated wake found a head owned by an older round"
                );
            }
        }
        target = waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .handle_handoff(
                next.peer,
                FairHandoff::Continue(next.round),
                AfterProgress::Stop,
            );
    }
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
    // Diagnostic label only; FairWakeRound equality uses Arc identity.
    next_round_sequence: u64,
    // Invariant: at most one round owns a fixed, duplicate-free peer snapshot.
    // Releases during that round coalesce into one repeat scan through pending_wake.
    active_round: Option<WakeRound>,
    pending_wake: bool,
}

impl InboundWaitQueues {
    pub(super) fn queue_for_peer(&mut self, peer: Option<Did>) -> Arc<CoordinatedFairWaitQueue> {
        let peer = PeerKey::from(peer);
        if let Some(queue) = self.existing_queue_by_key(peer) {
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
        peer: Option<Did>,
    ) -> Option<Arc<CoordinatedFairWaitQueue>> {
        self.existing_queue_by_key(PeerKey::from(peer))
    }

    fn existing_queue_by_key(&mut self, peer: PeerKey) -> Option<Arc<CoordinatedFairWaitQueue>> {
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

    pub(super) fn front_demands(&mut self) -> Vec<FairCapacityDemand> {
        self.prune_expired();
        self.queues
            .values()
            .filter_map(Weak::upgrade)
            .filter_map(|queue| queue.front_demand())
            .collect()
    }

    fn begin_round(&mut self) -> Option<WakeTarget> {
        self.prune_expired();
        let candidates = self.round_candidates();
        if candidates.is_empty() {
            return None;
        }
        let round = FairWakeRound::new(self.next_round_sequence);
        self.next_round_sequence = self.next_round_sequence.wrapping_add(1);
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
            let Some(queue) = self.existing_queue_by_key(peer) else {
                continue;
            };
            if let Some(active) = self.active_round.as_mut() {
                active.current = Some(peer);
            }
            self.last_woken = Some(peer);
            return Some(WakeTarget {
                peer: peer.into_peer(),
                queue,
                round,
            });
        }
    }

    fn matches_active(&self, peer: Option<Did>, round: &FairWakeRound) -> bool {
        self.active_round.as_ref().is_some_and(|active| {
            &active.id == round && active.current == Some(PeerKey::from(peer))
        })
    }

    fn close_round(&mut self) -> bool {
        self.active_round = None;
        std::mem::take(&mut self.pending_wake)
    }

    fn finish_exhausted_round(&mut self) -> Option<WakeTarget> {
        if self.close_round() {
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
        peer: Option<Did>,
        handoff: FairHandoff,
        after_progress: AfterProgress,
    ) -> Option<WakeTarget> {
        let (round, made_progress) = match handoff {
            FairHandoff::HeadAdvanced => return self.request_wake_round(),
            FairHandoff::Continue(round) => (round, false),
            FairHandoff::Progress(round) => (round, true),
        };
        if !self.matches_active(peer, &round) {
            tracing::warn!(?peer, ?round, "ignored stale inbound wake handoff");
            return None;
        }
        if made_progress {
            let _coalesced_release = self.close_round();
            return match after_progress {
                AfterProgress::Stop => None,
                AfterProgress::Scan => self.begin_round(),
            };
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
