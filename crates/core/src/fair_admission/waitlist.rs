//! The waiting requests of a [`ResourceOrderedQueue`](super::ResourceOrderedQueue), indexed so
//! an event visits only the waiters whose blockers it may lift (load-bounds paper, Obligation
//! "Admission complexity", #938).
//!
//! Every ungranted waiter `w` is, as of its last evaluation, in one of two states:
//!
//! ```text
//!   blocked(w)  ⟺ ∃ v < w ungranted:  stream(v) = stream(w) ∨ hold(v) ∩ footprint(w) ≠ ∅
//!   refused(w)  ⟺ ¬blocked(w) ∧ try_admit(w) = Err(refusal(w))
//!   hold(v)     =  refusal(v)                       if v is refused
//!               =  { ρ | (ρ, shed) ∈ reliefs(v) }   if v is blocked, every blocker of v lies in
//!                                                    ⋃ { {ρ} ∪ shed | (ρ, shed) ∈ reliefs(v) }
//!               =  ∅                                otherwise
//! ```
//!
//! A waiter holds its stream, a refused one also its refusal. A footprint depends on the
//! ledger's state: a *relief* `(ρ, shed)` of a waiter names a resource `ρ` of its footprint whose
//! release may shed the resources `shed` from it (a lane's reservation, once it covers the
//! request, sheds the shared budget). A waiter blocked only on what its reliefs may lift holds
//! them: it is served first come first served on them, so a later request of smaller size does
//! not pass it there; a waiter blocked by its stream or by its own budget holds nothing, so no
//! one waits behind another's budget. The tests are range queries over ordered indices (waiters
//! by stream, holds by resource), so judging a waiter costs `O(log n)`. A blocked waiter is
//! *parked* under the one resource it was blocked by and its holds, a refused one under each
//! resource of its refusal.
//!
//! An event names the blockers that may have lifted: a release the resources it freed, a grant or
//! a withdrawal the holds and the stream of the waiter that left. Each is *scanned*: its parked
//! waiters are judged again in arrival order, and the scan stops at the first that ends parked
//! under the scanned resource again, since every later waiter parked there is blocked behind it
//! (stop rule). A judgement that grants, or shrinks a hold, names further blockers to scan.
//! So an event visits `O(g + k + r)` waiters at `O(log n)` each: `g` the grants, `r` the
//! resources it named, and `k` the waiters a scan judged and parked under another blocker. Each
//! of those moves is charged to the freeing of the blocker it was parked under, and happens at
//! most once per such freeing.
//!
//! Invariant (one refusal per resource): a refused waiter never has an earlier refused waiter
//! whose refusal shares a resource with its own and differs from it. When a waiter is refused,
//! every later refused waiter it blocks that way is *demoted*, to blocked on the shared resource,
//! which lifts the rest of its refusal. Then a later refusal that shares the resource a scan stops
//! at equals the stopping one, and blocks nothing the stopping one does not: the stop rule loses
//! no waiter. A demotion is charged to the refusal it ends, once.
//!
//! Law (refusals are budgets): an [`AdmissionLedger`] refuses by naming one budget's resources,
//! the same set whenever that budget refuses, and a resource belongs to a bounded number of
//! budgets, so the refusals that share a resource are few, and are indexed by their set.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use super::resource_order::AdmissionLedger;

/// The position of a waiter in arrival order.
pub(crate) type Arrival = u64;

/// One waiting request and where it is indexed.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Waiter<Q, R> {
    /// The request.
    request: Q,
    /// Its stream.
    stream: R,
    /// The resources of its refusal, if it was refused; empty if blocked.
    refusal: Vec<R>,
    /// The reliefs it holds while blocked; empty if refused.
    holds: Vec<R>,
    /// The resources it is parked under.
    parked: Vec<R>,
}

impl<Q, R: Clone> Waiter<Q, R> {
    /// What it holds against later waiters besides its stream: its refusal or its reliefs.
    fn held(&self) -> impl Iterator<Item = R> + '_ {
        self.refusal.iter().chain(self.holds.iter()).cloned()
    }
}

/// A deliberately broken discipline, for the model to refute.
#[cfg(all(test, not(target_family = "wasm")))]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Mutant {
    /// One first-in-first-out queue: every waiter after an ungranted one is blocked.
    Fifo,
    /// No waiter is ever blocked by an earlier refusal.
    IgnoreRefusals,
    /// A blocked waiter does not hold its stream.
    SkipFreesStream,
    /// A withdrawal scans nothing.
    WithdrawWithoutScan,
    /// A release scans nothing.
    ReleaseWithoutScan,
    /// A blocked waiter holds its reliefs whatever blocks it, its own budget included.
    HoldWhateverBlocks,
}

/// The waiters of a resource-ordered queue (module documentation).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Waitlist<Q, R: Ord> {
    /// The ungranted waiters, in arrival order.
    waiters: BTreeMap<Arrival, Waiter<Q, R>>,
    /// The ungranted waiters of each stream.
    streams: BTreeMap<R, BTreeSet<Arrival>>,
    /// The refused waiters, by each resource of their refusal and then by the refusal.
    refused: BTreeMap<R, BTreeMap<Vec<R>, BTreeSet<Arrival>>>,
    /// The blocked waiters holding each relief.
    holding: BTreeMap<R, BTreeSet<Arrival>>,
    /// The waiters parked under each resource.
    parked: BTreeMap<R, BTreeSet<Arrival>>,
    /// The arrival the next waiter gets.
    next: Arrival,
    /// The broken discipline the model checks, if any.
    #[cfg(all(test, not(target_family = "wasm")))]
    mutant: Option<Mutant>,
}

impl<Q, R: Ord + Clone> Default for Waitlist<Q, R> {
    fn default() -> Self {
        Self {
            waiters: BTreeMap::new(),
            streams: BTreeMap::new(),
            refused: BTreeMap::new(),
            holding: BTreeMap::new(),
            parked: BTreeMap::new(),
            next: 0,
            #[cfg(all(test, not(target_family = "wasm")))]
            mutant: None,
        }
    }
}

/// What one event did: the waiters it granted, with their grants, and the waiters it judged.
pub(crate) struct Served<G> {
    /// The waiters granted, by arrival.
    pub(crate) granted: Vec<(Arrival, G)>,
    /// The waiters judged.
    pub(crate) visits: u64,
}

impl<Q, R: Ord + Clone> Waitlist<Q, R> {
    /// The broken discipline `mutant`, for the model.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn mutant(mutant: Mutant) -> Self {
        Self {
            mutant: Some(mutant),
            ..Self::default()
        }
    }

    /// Whether the discipline is `mutant`.
    #[cfg(all(test, not(target_family = "wasm")))]
    fn is(&self, mutant: Mutant) -> bool {
        self.mutant == Some(mutant)
    }

    /// The number of ungranted waiters.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn len(&self) -> usize {
        self.waiters.len()
    }

    /// The ungranted waiters' requests, in arrival order.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn requests(&self) -> impl Iterator<Item = (Arrival, &Q)> {
        self.waiters
            .iter()
            .map(|(arrival, waiter)| (*arrival, &waiter.request))
    }

    /// The refused waiters' requests, in arrival order.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn refused(&self) -> impl Iterator<Item = (Arrival, &Q)> {
        self.waiters
            .iter()
            .filter(|(_, waiter)| !waiter.refusal.is_empty())
            .map(|(arrival, waiter)| (*arrival, &waiter.request))
    }

    /// Queue `request` and judge it, with whatever its grant unblocks: its arrival and what the
    /// event did.
    pub(crate) fn arrive<L>(&mut self, ledger: &L, request: Q) -> (Arrival, Served<L::Grant>)
    where L: AdmissionLedger<Request = Q, Resource = R> {
        let arrival = self.next;
        self.next = arrival.saturating_add(1);
        let stream = ledger.stream(&request);
        self.streams
            .entry(stream.clone())
            .or_default()
            .insert(arrival);
        self.waiters.insert(arrival, Waiter {
            request,
            stream,
            refusal: Vec::new(),
            holds: Vec::new(),
            parked: Vec::new(),
        });
        let mut served = Served {
            granted: Vec::new(),
            visits: 0,
        };
        let mut scans = BTreeSet::new();
        self.judge(ledger, arrival, &mut scans, &mut served);
        self.scan(ledger, scans, &mut served);
        (arrival, served)
    }

    /// The ledger freed `resources`: scan them.
    pub(crate) fn release<L>(
        &mut self,
        ledger: &L,
        resources: impl IntoIterator<Item = R>,
    ) -> Served<L::Grant>
    where
        L: AdmissionLedger<Request = Q, Resource = R>,
    {
        let mut served = Served {
            granted: Vec::new(),
            visits: 0,
        };
        #[cfg(all(test, not(target_family = "wasm")))]
        if self.is(Mutant::ReleaseWithoutScan) {
            return served;
        }
        self.scan(ledger, resources.into_iter().collect(), &mut served);
        served
    }

    /// Withdraw the waiter `arrival`, if it waits, and scan what it blocked: whether it waited,
    /// and what the event did.
    pub(crate) fn withdraw<L>(&mut self, ledger: &L, arrival: Arrival) -> (bool, Served<L::Grant>)
    where L: AdmissionLedger<Request = Q, Resource = R> {
        let mut served = Served {
            granted: Vec::new(),
            visits: 0,
        };
        let Some(waiter) = self.remove(arrival) else {
            return (false, served);
        };
        #[cfg(all(test, not(target_family = "wasm")))]
        if self.is(Mutant::WithdrawWithoutScan) {
            return (true, served);
        }
        let scans = waiter.held().chain([waiter.stream.clone()]).collect();
        self.scan(ledger, scans, &mut served);
        (true, served)
    }

    /// Scan `scans`, and every blocker a judgement adds, to their fixpoint (module
    /// documentation).
    fn scan<L>(&mut self, ledger: &L, mut scans: BTreeSet<R>, served: &mut Served<L::Grant>)
    where L: AdmissionLedger<Request = Q, Resource = R> {
        while let Some(resource) = scans.pop_first() {
            let mut after = None;
            loop {
                let next = self.parked.get(&resource).and_then(|parked| match after {
                    Some(after) => parked.range((after + 1)..).next().copied(),
                    None => parked.first().copied(),
                });
                let Some(arrival) = next else {
                    break;
                };
                self.judge(ledger, arrival, &mut scans, served);
                let parked_again = self
                    .waiters
                    .get(&arrival)
                    .is_some_and(|waiter| waiter.parked.contains(&resource));
                if parked_again {
                    break;
                }
                after = Some(arrival);
            }
        }
    }

    /// Judge the waiter `arrival` again: grant it, or index it as refused or blocked; add to
    /// `scans` the blockers its new state lifts.
    fn judge<L>(
        &mut self,
        ledger: &L,
        arrival: Arrival,
        scans: &mut BTreeSet<R>,
        served: &mut Served<L::Grant>,
    ) where
        L: AdmissionLedger<Request = Q, Resource = R>,
    {
        served.visits = served.visits.saturating_add(1);
        let Some(waiter) = self.waiters.get(&arrival) else {
            return;
        };
        let stream = waiter.stream.clone();
        let old_held = waiter.held().collect::<Vec<_>>();
        self.unindex(arrival);
        let blockers = self.blockers(ledger, arrival, &stream);
        if let Some(blocker) = blockers.first().cloned() {
            let holds = self.holds(ledger, arrival, &blockers);
            scans.extend(old_held.into_iter().filter(|old| !holds.contains(old)));
            let parked = [blocker]
                .into_iter()
                .chain(holds.iter().cloned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            self.park(arrival, parked, Vec::new(), holds);
            return;
        }
        let Some(waiter) = self.waiters.get(&arrival) else {
            return;
        };
        match ledger.try_admit(&waiter.request) {
            Ok(grant) => {
                self.remove(arrival);
                scans.extend(old_held);
                scans.insert(stream);
                served.granted.push((arrival, grant));
            }
            Err(refused) => {
                let refusal = refused.into_iter().collect::<Vec<_>>();
                scans.extend(old_held.into_iter().filter(|old| !refusal.contains(old)));
                self.park(arrival, refusal.clone(), refusal.clone(), Vec::new());
                self.demote_after(arrival, &refusal, scans, served);
            }
        }
    }

    /// The resources earlier waiters block `arrival` by, its stream first: the stream an
    /// earlier waiter of it holds, and each resource of its footprint an earlier refusal or
    /// relief holds (pure).
    fn blockers<L>(&self, ledger: &L, arrival: Arrival, stream: &R) -> Vec<R>
    where L: AdmissionLedger<Request = Q, Resource = R> {
        let earlier = |index: &BTreeMap<R, BTreeSet<Arrival>>, resource: &R| {
            index
                .get(resource)
                .is_some_and(|arrivals| arrivals.range(..arrival).next().is_some())
        };
        let refused_earlier = |resource: &R| {
            self.refused.get(resource).is_some_and(|refusals| {
                refusals
                    .values()
                    .any(|arrivals| arrivals.range(..arrival).next().is_some())
            })
        };
        #[cfg(all(test, not(target_family = "wasm")))]
        if self.is(Mutant::Fifo) && self.waiters.range(..arrival).next().is_some() {
            return vec![stream.clone()];
        }
        #[cfg(all(test, not(target_family = "wasm")))]
        let streams_held = !self.is(Mutant::SkipFreesStream);
        #[cfg(not(all(test, not(target_family = "wasm"))))]
        let streams_held = true;
        let stream_blocks = match streams_held {
            true => earlier(&self.streams, stream),
            // The mutant: only a refused waiter holds its stream.
            false => self
                .waiters
                .range(..arrival)
                .any(|(_, waiter)| waiter.stream == *stream && !waiter.refusal.is_empty()),
        };
        let mut blockers = Vec::from_iter(stream_blocks.then(|| stream.clone()));
        #[cfg(all(test, not(target_family = "wasm")))]
        if self.is(Mutant::IgnoreRefusals) {
            return blockers;
        }
        if let Some(waiter) = self.waiters.get(&arrival) {
            blockers.extend(
                ledger
                    .footprint(&waiter.request)
                    .into_iter()
                    .filter(|resource| {
                        refused_earlier(resource) || earlier(&self.holding, resource)
                    }),
            );
        }
        blockers
    }

    /// The reliefs the blocked waiter `arrival` holds: all of them when every one of its
    /// `blockers` lies in what they may lift, none otherwise (module documentation).
    fn holds<L>(&self, ledger: &L, arrival: Arrival, blockers: &[R]) -> Vec<R>
    where L: AdmissionLedger<Request = Q, Resource = R> {
        let Some(waiter) = self.waiters.get(&arrival) else {
            return Vec::new();
        };
        let reliefs = ledger.reliefs(&waiter.request);
        let liftable = |blocker: &R| {
            reliefs
                .iter()
                .any(|(relief, shed)| relief == blocker || shed.contains(blocker))
        };
        #[cfg(all(test, not(target_family = "wasm")))]
        let guarded = !self.is(Mutant::HoldWhateverBlocks);
        #[cfg(not(all(test, not(target_family = "wasm"))))]
        let guarded = true;
        match !guarded || blockers.iter().all(liftable) {
            true => reliefs.into_iter().map(|(relief, _)| relief).collect(),
            false => Vec::new(),
        }
    }

    /// Demote every waiter after `arrival` refused by a set that shares a resource with
    /// `refusal` and differs from it: `arrival` blocks it on that resource (the invariant), so it
    /// waits blocked there, and the rest of its refusal is scanned.
    fn demote_after<G>(
        &mut self,
        arrival: Arrival,
        refusal: &[R],
        scans: &mut BTreeSet<R>,
        served: &mut Served<G>,
    ) {
        #[cfg(all(test, not(target_family = "wasm")))]
        if self.is(Mutant::IgnoreRefusals) {
            return;
        }
        for resource in refusal {
            let demoted = self
                .refused
                .get(resource)
                .into_iter()
                .flat_map(|refusals| refusals.iter())
                .filter(|(other, _)| other.as_slice() != refusal)
                .flat_map(|(other, arrivals)| {
                    arrivals
                        .range((arrival + 1)..)
                        .map(move |later| (*later, other.clone()))
                })
                .collect::<Vec<_>>();
            for (later, other) in demoted {
                served.visits = served.visits.saturating_add(1);
                self.unindex(later);
                self.park(later, vec![resource.clone()], Vec::new(), Vec::new());
                scans.extend(other.into_iter().filter(|lifted| lifted != resource));
            }
        }
    }

    /// Index `arrival` as parked under `parked`, refused by `refusal` (empty when blocked),
    /// holding `holds` (empty when refused).
    fn park(&mut self, arrival: Arrival, parked: Vec<R>, refusal: Vec<R>, holds: Vec<R>) {
        for resource in &holds {
            self.holding
                .entry(resource.clone())
                .or_default()
                .insert(arrival);
        }
        for resource in &parked {
            self.parked
                .entry(resource.clone())
                .or_default()
                .insert(arrival);
        }
        for resource in &refusal {
            self.refused
                .entry(resource.clone())
                .or_default()
                .entry(refusal.clone())
                .or_default()
                .insert(arrival);
        }
        if let Some(waiter) = self.waiters.get_mut(&arrival) {
            waiter.parked = parked;
            waiter.refusal = refusal;
            waiter.holds = holds;
        }
    }

    /// Take `arrival` out of the parked and refused indices.
    fn unindex(&mut self, arrival: Arrival) {
        let Some(waiter) = self.waiters.get_mut(&arrival) else {
            return;
        };
        let parked = std::mem::take(&mut waiter.parked);
        let refusal = std::mem::take(&mut waiter.refusal);
        let holds = std::mem::take(&mut waiter.holds);
        for resource in holds {
            remove_from(&mut self.holding, &resource, arrival);
        }
        for resource in parked {
            remove_from(&mut self.parked, &resource, arrival);
        }
        for resource in &refusal {
            if let Some(refusals) = self.refused.get_mut(resource) {
                remove_from(refusals, &refusal, arrival);
                if refusals.is_empty() {
                    self.refused.remove(resource);
                }
            }
        }
    }

    /// Remove the waiter `arrival` from the waitlist and every index.
    fn remove(&mut self, arrival: Arrival) -> Option<Waiter<Q, R>> {
        let waiter = self.waiters.get(&arrival)?;
        let (refusal, holds) = (waiter.refusal.clone(), waiter.holds.clone());
        self.unindex(arrival);
        let mut waiter = self.waiters.remove(&arrival)?;
        remove_from(&mut self.streams, &waiter.stream, arrival);
        waiter.refusal = refusal;
        waiter.holds = holds;
        Some(waiter)
    }
}

/// Remove `arrival` from the set under `key` of `index`, and the set once it is empty.
fn remove_from<K: Ord>(index: &mut BTreeMap<K, BTreeSet<Arrival>>, key: &K, arrival: Arrival) {
    if let Some(set) = index.get_mut(key) {
        set.remove(&arrival);
        if set.is_empty() {
            index.remove(key);
        }
    }
}
