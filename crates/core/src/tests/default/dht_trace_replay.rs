//! Tier 2 trace-validation — a synchronous reference executor that runs the
//! discovery/stabilization protocol with the **real** `PeerRing` operations,
//! including the production multi-hop `find_successor` routing (not the
//! single-hop spec abstraction the Stateright stage-2 model uses).
//!
//! Each node is a real `PeerRing`; messages mirror the production handlers
//! (`handlers/{connection,stabilization}.rs`): join handshakes
//! (`FindSuccessorForConnect`), `find_successor` forwarding via
//! `RemoteAction`/`closest_predecessor`, `notify`/report, and `fix_fingers`
//! (`FindSuccessorForFix`). A deterministic queue drives it; run to quiescence
//! over enough stabilization rounds it converges every node to the `spec`
//! fixpoint. This closes the gap the stage-2 abstraction leaves open: the real
//! routing is exercised here and reaches the same converged DHT.

use std::collections::BTreeSet;
use std::collections::VecDeque;

use num_bigint::BigUint;

use super::dht_convergence::spec;
use super::dht_convergence::K;
use crate::dht::successor::SuccessorReader;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::storage::MemStorage;

fn did_frac(num: u64, den: u64) -> Did {
    Did::from((BigUint::from(1u8) << 160) * BigUint::from(num) / BigUint::from(den))
}

/// Why a `find_successor` was issued — determines what the origin does with the
/// answer. Both currently end in a connection, mirroring the real handlers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Purpose {
    /// `FindSuccessorForConnect` — origin connects to the answer.
    Connect,
    /// `FindSuccessorForFix` — origin connects to the answer (fixes its finger).
    Fix,
}

#[derive(Clone, Debug)]
enum Msg {
    /// `FindSuccessorSend`: find the successor of `target`, reply to `origin`.
    FindSucc {
        target: Did,
        origin: usize,
        purpose: Purpose,
    },
    /// `FindSuccessorReport`: `node` is the discovered successor.
    FindReport { node: Did, origin: usize },
    /// `NotifyPredecessorSend`.
    Notify { from: usize },
    /// `NotifyPredecessorReport`: the recipient connects to `pred`.
    NotifyReport { pred: Did },
}

/// The reference network: one real `PeerRing` per node + a deterministic queue.
struct RealSim {
    all: Vec<Did>,
    rings: Vec<PeerRing>,
    connected: Vec<BTreeSet<usize>>,
    queue: VecDeque<(usize, Msg)>,
}

impl RealSim {
    fn new(all: Vec<Did>) -> Self {
        let rings = all
            .iter()
            .map(|&d| PeerRing::new_with_storage(d, K as u8, Box::new(MemStorage::new())))
            .collect();
        let n = all.len();
        Self {
            all,
            rings,
            connected: vec![BTreeSet::new(); n],
            queue: VecDeque::new(),
        }
    }

    fn idx(&self, did: Did) -> usize {
        self.all.iter().position(|&d| d == did).expect("did in set")
    }

    /// Establish a (symmetric) connection: both sides `join` each other directly
    /// (a connected peer is known locally) and each issues its
    /// `FindSuccessorForConnect` handshake to discover transitive successors —
    /// exactly `manually_establish_connection` + the join protocol.
    fn connect(&mut self, a: usize, b: usize) {
        if a == b || self.connected[a].contains(&b) {
            return;
        }
        self.connected[a].insert(b);
        self.connected[b].insert(a);
        let _ = self.rings[a].join(self.all[b]);
        let _ = self.rings[b].join(self.all[a]);
        self.queue.push_back((b, Msg::FindSucc {
            target: self.all[a],
            origin: a,
            purpose: Purpose::Connect,
        }));
        self.queue.push_back((a, Msg::FindSucc {
            target: self.all[b],
            origin: b,
            purpose: Purpose::Connect,
        }));
    }

    /// One `stabilize()`: notify successors + one `fix_fingers` step (real ops).
    fn stabilize(&mut self, m: usize) {
        for s in self.rings[m].successors().list().unwrap() {
            let si = self.idx(s);
            if si != m {
                self.queue.push_back((si, Msg::Notify { from: m }));
            }
        }
        if let Ok(PeerRingAction::RemoteAction(
            next,
            PeerRingRemoteAction::FindSuccessorForFix(finger_did),
        )) = self.rings[m].fix_fingers()
        {
            let ni = self.idx(next);
            if ni != m {
                self.queue.push_back((ni, Msg::FindSucc {
                    target: finger_did,
                    origin: m,
                    purpose: Purpose::Fix,
                }));
            }
        }
    }

    fn process(&mut self, dst: usize, msg: Msg) {
        match msg {
            // FindSuccessorSend handler: answer locally, or forward to the
            // closest preceding node — the production multi-hop routing.
            Msg::FindSucc {
                target,
                origin,
                purpose,
            } => match self.rings[dst].find_successor(target) {
                Ok(PeerRingAction::Some(node)) => {
                    self.queue
                        .push_back((origin, Msg::FindReport { node, origin }));
                }
                Ok(PeerRingAction::RemoteAction(next, _)) => {
                    let ni = self.idx(next);
                    if ni != dst {
                        self.queue.push_back((ni, Msg::FindSucc {
                            target,
                            origin,
                            purpose,
                        }));
                    } else if let Ok(s) = self.rings[dst].successors().min() {
                        self.queue
                            .push_back((origin, Msg::FindReport { node: s, origin }));
                    }
                }
                _ => {}
            },
            // FindSuccessorReport handler: connect to the discovered node.
            Msg::FindReport { node, origin } => {
                let ni = self.idx(node);
                self.connect(origin, ni);
            }
            // NotifyPredecessorSend handler: apply notify, report back unless the
            // sender already is the predecessor.
            Msg::Notify { from } => {
                let from_did = self.all[from];
                if let Ok(np) = self.rings[dst].notify(from_did) {
                    if np != from_did {
                        self.queue.push_back((from, Msg::NotifyReport { pred: np }));
                    }
                }
            }
            // NotifyPredecessorReport handler: connect to the reported pred.
            Msg::NotifyReport { pred } => {
                let pi = self.idx(pred);
                self.connect(dst, pi);
            }
        }
    }

    /// Process the queue to quiescence (with a generous safety cap).
    fn drain(&mut self) {
        let mut steps = 0usize;
        while let Some((dst, msg)) = self.queue.pop_front() {
            self.process(dst, msg);
            steps += 1;
            assert!(steps < 5_000_000, "runaway message loop");
        }
    }

    /// Star bootstrap, then `rounds` stabilization rounds, each drained fully.
    fn run(&mut self, rounds: usize) {
        for i in 1..self.all.len() {
            self.connect(0, i);
        }
        self.drain();
        for _ in 0..rounds {
            for m in 0..self.all.len() {
                self.stabilize(m);
            }
            self.drain();
        }
    }

    fn converged(&self) -> bool {
        (0..self.all.len()).all(|i| {
            self.rings[i].successors().list().unwrap() == spec::successors(&self.all, self.all[i])
                && *self.rings[i].lock_predecessor().unwrap()
                    == spec::predecessor(&self.all, self.all[i])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real-`find_successor` reference executor converges the star bootstrap
    /// to the `spec` fixpoint — i.e. the production multi-hop routing reaches the
    /// same converged DHT the deterministic/abstract tests assert. This is the
    /// Tier-2 trace-validation that closes the routing gap the stage-2 Stateright
    /// abstraction leaves open.
    #[test]
    fn real_find_successor_routing_converges_from_star() {
        for n in 2..=5u64 {
            let all: Vec<Did> = (0..n).map(|i| did_frac(i, n)).collect();
            let mut sim = RealSim::new(all);
            sim.run(200);
            assert!(sim.converged(), "real-op sim did not converge for n={n}");
        }
    }
}
