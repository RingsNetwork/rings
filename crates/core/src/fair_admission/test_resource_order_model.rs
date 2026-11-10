//! Model check of resource-ordered admission over every interleaving of a fixed workload.
//!
//! The ledger is an abstract node budget with one reserved lane, a shared pool and a budget
//! per peer, the shape of the inbound mailbox, whose budgets bound themselves (its gates are
//! the ledger's own, so they are not a property here); its footprint, reliefs, and refusals are
//! mechanically checked against the production [`InboundCapacity`] implementation of
//! [`AdmissionLedger`] in [`test_inbound_capacity_refines_the_model_ledger`]:
//!
//! ```text
//!   Ledger  = ℕ^lanes (admitted units) × ℕ^peers (admitted units)
//!   fits(r) ⟺ peer(r) + size(r) ≤ PEER_CAP
//!            ∧ Σ lanes + size(r) ≤ CAP − Σ_{ℓ ≠ lane(r)} (RES(ℓ) ∸ lane(ℓ))
//!   State   = Ledger × queue (arrival order, granted?) × arrived × held × resolved × cancels
//! ```
//!
//! Actions are the arrival of any request not yet arrived, a waiter claiming its grant, the
//! release of a claimed grant, and (once) the cancellation of a waiter, granted or not. Every
//! arrival, release and cancellation is an event of the [`Waitlist`] under check, which judges
//! the waiters it may unblock; the state holds the waitlist with its indices.
//!
//! Properties:
//!
//! - `Always` no overtaking: no event grants a request whose footprint meets the refusal, judged
//!   on the ledger as the grant found it, of an earlier waiter the discipline holds refused;
//! - `Always` stream order: no granted request has an earlier request of its stream still
//!   waiting ungranted;
//! - `Always` independence (no lost wake-up): no waiter is left ungranted that the ledger
//!   would admit, unless an earlier ungranted waiter shares its stream, is refused, judged on
//!   the ledger on its own, by a budget that meets its footprint, or holds their lane's
//!   reservation: the reservation would cover it once idle but does not now, and neither its
//!   stream nor its own peer's budget holds it back. Each earlier waiter is judged
//!   independently, never through the waiters a pass skipped, so it is not a pass restated;
//! - `Eventually` resolved: every path ends with every request released, refused at a full
//!   waitlist budget, or cancelled;
//! - `Sometimes` a later request passes a waiting one, a waiter is queued, a waiter is refused
//!   when the waitlist budget is full, and all are resolved, so the safety properties are not
//!   vacuous.
//!
//! Each property is shown not to be vacuous by a mutant discipline that the checker refutes:
//! one first-in-first-out queue (independence), admission that ignores refusals (no overtaking), a
//! blocked request that does not hold its stream (stream order), a withdrawal that scans nothing
//! (eventually resolved), a release that scans nothing (independence), and a request that holds
//! its lane while its own peer's budget blocks it (independence: another peer waits behind that
//! budget).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use stateright::Checker;
use stateright::Model;
use stateright::Property;

use super::resource_order::AdmissionLedger;
use super::wait_queue::FairWaitBudget;
use super::waitlist::Arrival;
use super::waitlist::Mutant;
use super::waitlist::Served;
use super::waitlist::Waitlist;
use crate::dht::Did;
use crate::swarm::callback::InboundCapacity;
use crate::swarm::callback::InboundCapacityPermit;
use crate::swarm::callback::InboundLane;
use crate::swarm::callback::InboundRequest;
use crate::swarm::callback::InboundResource;
use crate::swarm::callback::INBOUND_MODEL_LANES;
use crate::swarm::callback::INBOUND_MODEL_MAILBOX_BYTES;
use crate::swarm::callback::INBOUND_MODEL_PEER_BYTES;
use crate::swarm::callback::INBOUND_MODEL_RESERVED_BYTES_PER_LANE;

/// The node budget, in units.
const CAP: u8 = 3;
/// The fixed reservation of each lane: lane 0 (control) holds one unit for itself.
const RES: [u8; 2] = [1, 0];
/// Each peer's budget, in units.
const PEER_CAP: u8 = 2;

/// One request of the workload.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Request {
    /// The request's index in the workload.
    id: u8,
    /// The sending peer.
    peer: u8,
    /// The lane.
    lane: u8,
    /// The units it reserves.
    size: u8,
}

/// The workload: a large borrower and a small one on the unreserved lane, a control stream of
/// a borrower and then a reserved-size request, and a second peer's control request.
const WORKLOAD: [Request; 5] = [
    request_of(0, 1, 1, 2),
    request_of(1, 0, 1, 1),
    request_of(2, 0, 0, 2),
    request_of(3, 0, 0, 1),
    request_of(4, 1, 0, 1),
];

/// Request `id` of `peer` on `lane` for `size` units.
const fn request_of(id: u8, peer: u8, lane: u8, size: u8) -> Request {
    Request {
        id,
        peer,
        lane,
        size,
    }
}

/// A resource of the abstract ledger.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Resource {
    /// A peer's arrivals on a lane.
    Stream(u8, u8),
    /// A peer's budget.
    Peer(u8),
    /// A lane's share of the node budget.
    Lane(u8),
    /// The node budget beyond the reservations.
    Shared,
}

/// The admitted units.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
struct Ledger {
    /// Units admitted per lane.
    lanes: [u8; 2],
    /// Units admitted per peer.
    peers: [u8; 2],
}

impl Ledger {
    /// The units the other lanes' unmet reservations hold back from `lane`.
    fn held_back(&self, lane: u8) -> u8 {
        (0..2u8)
            .filter(|other| *other != lane)
            .map(|other| RES[usize::from(other)].saturating_sub(self.lanes[usize::from(other)]))
            .sum()
    }

    /// Whether `request`'s lane reservation alone covers it.
    fn covers(&self, request: &Request) -> bool {
        self.lanes[usize::from(request.lane)] + request.size <= RES[usize::from(request.lane)]
    }

    /// Whether `request`'s lane reservation does not cover it now but would once idle: a
    /// release on its lane may shed `Shared` from its footprint.
    fn relieved_by_lane(&self, request: &Request) -> bool {
        !self.covers(request) && request.size <= RES[usize::from(request.lane)]
    }

    /// Whether `request`'s own budget, its peer's, admits it.
    fn peer_admits(&self, request: &Request) -> bool {
        self.peers[usize::from(request.peer)] + request.size <= PEER_CAP
    }

    /// Admit `request`, or name the refusing resources (peer first, as in production).
    fn admit(&mut self, request: &Request) -> Result<(), Vec<Resource>> {
        if self.peers[usize::from(request.peer)] + request.size > PEER_CAP {
            return Err(vec![Resource::Peer(request.peer)]);
        }
        let total: u8 = self.lanes.iter().sum();
        if total + request.size > CAP.saturating_sub(self.held_back(request.lane)) {
            return Err(vec![Resource::Lane(request.lane), Resource::Shared]);
        }
        self.lanes[usize::from(request.lane)] += request.size;
        self.peers[usize::from(request.peer)] += request.size;
        Ok(())
    }

    /// Every budget's resources that refuse `request`, whichever production checks first: a
    /// waiter refused by two budgets holds both, though it was told of one.
    fn refusing(&self, request: &Request) -> Vec<Resource> {
        let mut refusing = Vec::new();
        if !self.peer_admits(request) {
            refusing.push(Resource::Peer(request.peer));
        }
        let total: u8 = self.lanes.iter().sum();
        if total + request.size > CAP.saturating_sub(self.held_back(request.lane)) {
            refusing.extend([Resource::Lane(request.lane), Resource::Shared]);
        }
        refusing
    }

    /// Release `request`'s units.
    fn release(&mut self, request: &Request) {
        self.lanes[usize::from(request.lane)] -= request.size;
        self.peers[usize::from(request.peer)] -= request.size;
    }
}

/// One admission attempt of an event: the request, the ledger it was judged on, and whether it
/// was admitted.
type Attempt = (Request, Ledger, bool);

/// The abstract ledger as an [`AdmissionLedger`], recording every attempt of an event.
struct ModelLedger {
    /// The admitted units.
    ledger: RefCell<Ledger>,
    /// The attempts of the event, in order.
    attempts: RefCell<Vec<Attempt>>,
}

impl ModelLedger {
    /// The footprint of `request` against `ledger`.
    fn footprint_of(ledger: &Ledger, request: &Request) -> Vec<Resource> {
        let mut footprint = vec![Resource::Peer(request.peer), Resource::Lane(request.lane)];
        if !ledger.covers(request) {
            footprint.push(Resource::Shared);
        }
        footprint
    }
}

impl AdmissionLedger for ModelLedger {
    type Grant = u8;
    type Request = Request;
    type Resource = Resource;
    type Resources = Vec<Resource>;

    fn stream(&self, request: &Request) -> Resource {
        Resource::Stream(request.peer, request.lane)
    }

    fn footprint(&self, request: &Request) -> Vec<Resource> {
        Self::footprint_of(&self.ledger.borrow(), request)
    }

    fn reliefs(&self, request: &Request) -> Vec<(Resource, Vec<Resource>)> {
        let ledger = self.ledger.borrow();
        Vec::from_iter(
            ledger
                .relieved_by_lane(request)
                .then(|| (Resource::Lane(request.lane), vec![Resource::Shared])),
        )
    }

    fn try_admit(&self, request: &Request) -> Result<u8, Vec<Resource>> {
        let before = *self.ledger.borrow();
        let outcome = self.ledger.borrow_mut().admit(request);
        self.attempts
            .borrow_mut()
            .push((*request, before, outcome.is_ok()));
        outcome.map(|()| request.id)
    }
}

/// A discipline: production, or a mutant that must be refuted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Discipline {
    /// The production [`Waitlist`].
    Production,
    /// A [`Waitlist`] broken by `Mutant`.
    Broken(Mutant),
    /// A waitlist cap check that fails to refuse when the wait budget is full.
    IgnoreWaitlistCap,
}

/// The state of the model.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct State {
    /// The admitted units.
    ledger: Ledger,
    /// The ungranted requests, as the discipline indexes them.
    waitlist: Waitlist<Request, Resource>,
    /// The requests granted and not yet claimed.
    granted: BTreeSet<u8>,
    /// The arrival of each request, once arrived.
    arrival: [Option<Arrival>; WORKLOAD.len()],
    /// Whether each workload request has arrived (queued, granted, or refused at a full waitlist).
    seen: [bool; WORKLOAD.len()],
    /// The requests holding a claimed grant.
    held: BTreeSet<u8>,
    /// The requests released, refused at a full waitlist, or cancelled.
    resolved: BTreeSet<u8>,
    /// Cancellations still allowed.
    cancels: u8,
    /// Count of arrivals refused because the waitlist budget was full.
    refused: u8,
    /// Whether some event granted a request past an earlier refusal it draws on.
    overtaken: bool,
}

impl State {
    /// Whether every request is released or cancelled.
    fn done(&self) -> bool {
        self.resolved.len() == WORKLOAD.len()
    }

    /// The ungranted requests, in arrival order.
    fn ungranted(&self) -> Vec<Request> {
        self.waitlist
            .requests()
            .map(|(_, request)| *request)
            .collect()
    }

    /// Whether a granted request arrived after a request still waiting ungranted that is
    /// `related` to it.
    fn granted_past_a_waiter(&self, related: impl Fn(&Request, &Request) -> bool) -> bool {
        let ungranted = self.ungranted();
        self.granted.iter().chain(self.held.iter()).any(|later| {
            ungranted.iter().any(|earlier| {
                related(earlier, &request(*later))
                    && self.arrival[usize::from(earlier.id)] < self.arrival[usize::from(*later)]
            })
        })
    }

    /// Whether some ungranted waiter is admissible on the ledger as it stands with nothing to
    /// justify its wait: no earlier ungranted waiter of its stream, and none whose refusal on
    /// that ledger, judged on its own, meets the waiter's footprint.
    ///
    /// Each earlier waiter is judged independently against the ledger, not by replaying the
    /// discipline: a waiter it blocked still counts by its own refusal, and nothing is inherited
    /// transitively from the waiters it blocked.
    fn admissible_waiter_left(&self) -> bool {
        let ungranted = self.ungranted();
        let refusal = |waiter: &Request| self.ledger.refusing(waiter);
        // An earlier waiter holds its lane's reservation against `waiter` when it is on that
        // lane, the reservation alone would admit it once covering it, and neither its stream
        // nor its own peer's budget holds it back.
        let holds_lane = |earlier: &Request, waiter: &Request| {
            let first_of_stream = ungranted
                .iter()
                .find(|other| (other.peer, other.lane) == (earlier.peer, earlier.lane))
                == Some(earlier);
            earlier.lane == waiter.lane
                && self.ledger.relieved_by_lane(earlier)
                && self.ledger.peer_admits(earlier)
                && first_of_stream
        };
        ungranted.iter().enumerate().any(|(position, waiter)| {
            let mut ledger = self.ledger;
            let footprint = ModelLedger::footprint_of(&self.ledger, waiter);
            let justified = ungranted.iter().take(position).any(|earlier| {
                (earlier.peer, earlier.lane) == (waiter.peer, waiter.lane)
                    || refusal(earlier)
                        .iter()
                        .any(|resource| footprint.contains(resource))
                    || holds_lane(earlier, waiter)
            });
            ledger.admit(waiter).is_ok() && !justified
        })
    }

    /// Whether a granted request arrived after a waiting one: independence, exercised.
    fn passed(&self) -> bool {
        self.granted_past_a_waiter(|_, _| true)
    }

    /// Whether a granted request has an earlier request of its stream still waiting.
    fn stream_reordered(&self) -> bool {
        self.granted_past_a_waiter(|earlier, later| {
            earlier.peer == later.peer && earlier.lane == later.lane
        })
    }
}

/// The workload request `id`.
fn request(id: u8) -> Request {
    WORKLOAD[usize::from(id)]
}

/// Whether some admission of `attempts` granted a request whose footprint meets the refusal,
/// judged on the ledger it was admitted on, of an earlier waiter of `refused`, the waiters the
/// discipline held refused when the event began, not granted before it in the event.
fn overtakes(attempts: &[Attempt], refused: &[Request]) -> bool {
    let mut granted = BTreeSet::new();
    for (request, before, admitted) in attempts {
        if !admitted {
            continue;
        }
        let footprint = ModelLedger::footprint_of(before, request);
        let passed = refused
            .iter()
            .take_while(|earlier| earlier.id != request.id)
            .filter(|earlier| !granted.contains(&earlier.id))
            .any(|earlier| {
                let mut ledger = *before;
                ledger
                    .admit(earlier)
                    .err()
                    .is_some_and(|refusal| refusal.iter().any(|r| footprint.contains(r)))
            });
        if passed {
            return true;
        }
        granted.insert(request.id);
    }
    false
}

/// The resources a release of `request` frees: its peer's budget, its lane's share and the
/// node budget, as the inbound mailbox names them.
fn freed(request: &Request) -> [Resource; 3] {
    [
        Resource::Peer(request.peer),
        Resource::Lane(request.lane),
        Resource::Shared,
    ]
}

/// Run `event` on `state`'s waitlist against its ledger, then record its grants and whether it
/// overtook.
fn event(
    state: &mut State,
    event: impl FnOnce(&mut Waitlist<Request, Resource>, &ModelLedger) -> Served<u8>,
) {
    let ledger = ModelLedger {
        ledger: RefCell::new(state.ledger),
        attempts: RefCell::new(Vec::new()),
    };
    let refused = state
        .waitlist
        .refused()
        .map(|(_, request)| *request)
        .collect::<Vec<_>>();
    let served = event(&mut state.waitlist, &ledger);
    // A waiter the event withdrew holds nothing against it.
    let left = |request: &&Request| {
        state
            .waitlist
            .requests()
            .any(|(_, waiting)| waiting.id == request.id)
            || served.granted.iter().any(|(_, id)| *id == request.id)
    };
    let refused = refused.iter().filter(left).copied().collect::<Vec<_>>();
    state.overtaken |= overtakes(&ledger.attempts.borrow(), &refused);
    state.ledger = ledger.ledger.into_inner();
    state
        .granted
        .extend(served.granted.into_iter().map(|(_, id)| id));
}

/// One transition of [`State`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Action {
    /// A request arrives and waits.
    Arrive(u8),
    /// A granted waiter claims its grant.
    Claim(u8),
    /// A claimed grant is released.
    Release(u8),
    /// A waiter is cancelled, releasing a grant it had not claimed.
    Cancel(u8),
}

/// The model under one discipline.
struct AdmissionModel {
    /// The discipline under check.
    discipline: Discipline,
    /// Optional hard cap on queued waiters, driven through [`FairWaitBudget`].
    waitlist_cap: Option<usize>,
    /// Whether to check liveness alone (the `eventually` property).
    liveness: bool,
}

impl AdmissionModel {
    /// Whether the wait budget admits one more queued waiter beyond `queued` existing waiters,
    /// evaluated through the production [`FairWaitBudget`].
    fn wait_budget_allows(&self, queued: usize) -> bool {
        if self.discipline == Discipline::IgnoreWaitlistCap {
            return true;
        }
        let Some(cap) = self.waitlist_cap else {
            return true;
        };
        let budget = Arc::new(FairWaitBudget::new(cap, cap));
        let mut held = Vec::with_capacity(queued);
        for _ in 0..queued {
            let Some(permit) = budget.try_acquire_for_test(1) else {
                return false;
            };
            held.push(permit);
        }
        budget.try_acquire_for_test(1).is_some()
    }
}

impl Model for AdmissionModel {
    type Action = Action;
    type State = State;

    fn init_states(&self) -> Vec<State> {
        let waitlist = match self.discipline {
            Discipline::Production | Discipline::IgnoreWaitlistCap => Waitlist::default(),
            Discipline::Broken(mutant) => Waitlist::mutant(mutant),
        };
        vec![State {
            ledger: Ledger::default(),
            waitlist,
            granted: BTreeSet::new(),
            arrival: [None; WORKLOAD.len()],
            seen: [false; WORKLOAD.len()],
            held: BTreeSet::new(),
            resolved: BTreeSet::new(),
            cancels: 1,
            refused: 0,
            overtaken: false,
        }]
    }

    fn actions(&self, state: &State, actions: &mut Vec<Action>) {
        for candidate in WORKLOAD
            .iter()
            .filter(|candidate| !state.seen[usize::from(candidate.id)])
        {
            actions.push(Action::Arrive(candidate.id));
        }
        for id in state.granted.iter().copied() {
            actions.push(Action::Claim(id));
        }
        if state.cancels > 0 {
            let waiting = state.waitlist.requests().map(|(_, request)| request.id);
            actions.extend(
                state
                    .granted
                    .iter()
                    .copied()
                    .chain(waiting)
                    .map(Action::Cancel),
            );
        }
        actions.extend(state.held.iter().copied().map(Action::Release));
    }

    fn next_state(&self, last: &State, action: Action) -> Option<State> {
        let mut state = last.clone();
        match action {
            Action::Arrive(id) => {
                state.seen[usize::from(id)] = true;
                let queued_before = state.waitlist.len();
                let mut probe = state.clone();
                let mut arrived = None;
                event(&mut probe, |waitlist, ledger| {
                    let (arrival, served) = waitlist.arrive(ledger, request(id));
                    arrived = Some(arrival);
                    served
                });
                let would_wait = probe
                    .waitlist
                    .requests()
                    .any(|(_, waiting)| waiting.id == id);
                if would_wait && !self.wait_budget_allows(queued_before) {
                    state.refused = state.refused.saturating_add(1);
                    state.resolved.insert(id);
                } else {
                    state = probe;
                    state.arrival[usize::from(id)] = arrived;
                }
            }
            Action::Claim(id) => {
                state.granted.remove(&id).then_some(())?;
                state.held.insert(id);
            }
            Action::Release(id) => {
                state.held.remove(&id).then_some(())?;
                state.ledger.release(&request(id));
                state.resolved.insert(id);
                event(&mut state, |waitlist, ledger| {
                    waitlist.release(ledger, freed(&request(id)))
                });
            }
            Action::Cancel(id) => {
                state.cancels -= 1;
                state.resolved.insert(id);
                let silent = self.discipline == Discipline::Broken(Mutant::WithdrawWithoutScan);
                if state.granted.remove(&id) {
                    state.ledger.release(&request(id));
                    if !silent {
                        event(&mut state, |waitlist, ledger| {
                            waitlist.release(ledger, freed(&request(id)))
                        });
                    }
                } else {
                    let arrival = state.arrival[usize::from(id)]?;
                    event(&mut state, |waitlist, ledger| {
                        waitlist.withdraw(ledger, arrival).1
                    });
                }
            }
        }
        Some(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        if self.liveness {
            return vec![Property::eventually("resolved", |_, state: &State| {
                state.done()
            })];
        }
        let mut properties = vec![
            Property::always("no overtaking", |_, state: &State| !state.overtaken),
            Property::always("stream order", |_, state: &State| !state.stream_reordered()),
            Property::always("independence", |_, state: &State| {
                !state.admissible_waiter_left()
            }),
            Property::<Self>::always("waitlist-full bound holds", |model, state: &State| {
                model
                    .waitlist_cap
                    .is_none_or(|cap| state.waitlist.len() <= cap)
            }),
            Property::always(
                "refused arrivals hold neither capacity nor waitlist slots",
                |_, state: &State| {
                    let accounted_lanes: u8 = state
                        .granted
                        .iter()
                        .chain(state.held.iter())
                        .map(|id| request(*id).size)
                        .sum();
                    state.ledger.lanes.iter().sum::<u8>() == accounted_lanes
                        && state
                            .waitlist
                            .requests()
                            .all(|(_, waiting)| !state.resolved.contains(&waiting.id))
                },
            ),
            Property::sometimes(
                "a later request passes a waiting one",
                |_, state: &State| state.passed(),
            ),
            Property::sometimes("a waiter is queued", |_, state: &State| {
                state.waitlist.len() > 0
            }),
            Property::sometimes("all resolved", |_, state: &State| state.done()),
        ];
        if self.waitlist_cap.is_some() {
            properties.push(Property::sometimes(
                "a waiter is refused when the waitlist is full",
                |_, state: &State| state.refused > 0,
            ));
        }
        properties
    }
}

/// Check `discipline`'s safety and its liveness under `waitlist_cap`, returning the names of
/// refuted properties.
fn refuted_with_cap(discipline: Discipline, waitlist_cap: Option<usize>) -> BTreeSet<&'static str> {
    let mut refuted = BTreeSet::new();
    for liveness in [false, true] {
        let checker = AdmissionModel {
            discipline,
            waitlist_cap,
            liveness,
        }
        .checker()
        .spawn_bfs()
        .join();
        for property in checker.model().properties() {
            let discovered = checker.discovery(property.name).is_some();
            let failed = match property.expectation {
                stateright::Expectation::Sometimes => !discovered,
                _ => discovered,
            };
            if failed {
                refuted.insert(property.name);
            }
        }
    }
    refuted
}

/// Check `discipline`'s safety and its liveness, returning the names of refuted properties.
fn refuted(discipline: Discipline) -> BTreeSet<&'static str> {
    refuted_with_cap(discipline, None)
}

/// The production pass satisfies every law over every interleaving of the workload, both with an
/// unbounded waitlist and when a bounded wait budget refuses excess waiters.
#[test]
fn test_resource_order_satisfies_its_laws() {
    assert_eq!(refuted(Discipline::Production), BTreeSet::new());
    assert_eq!(
        refuted_with_cap(Discipline::Production, Some(2)),
        BTreeSet::new()
    );
}

/// Each law is load-bearing: a mutant that drops it is refuted by exactly that law's check
/// (among others it may also break).
#[test]
fn test_each_resource_order_mutant_is_refuted() {
    for (mutant, law) in [
        (Mutant::Fifo, "independence"),
        (Mutant::IgnoreRefusals, "no overtaking"),
        (Mutant::SkipFreesStream, "stream order"),
        (Mutant::WithdrawWithoutScan, "resolved"),
        (Mutant::ReleaseWithoutScan, "independence"),
        (Mutant::HoldWhateverBlocks, "independence"),
    ] {
        let discipline = Discipline::Broken(mutant);
        assert!(
            refuted(discipline).contains(law),
            "{discipline:?} must be refuted by `{law}`"
        );
    }
    assert!(
        refuted_with_cap(Discipline::IgnoreWaitlistCap, Some(2))
            .contains("waitlist-full bound holds"),
        "IgnoreWaitlistCap must be refuted by `waitlist-full bound holds`"
    );
}

/// Differential refinement check: the production [`InboundCapacity`] implementation of
/// [`AdmissionLedger`] (`stream`, `footprint`, `reliefs`, `try_admit`, and permit `Drop`) agrees
/// step-for-step with the abstract ledger law across every admitted subset and across a
/// resource-ordered [`Waitlist`] trace exercising lane reservation, borrowing, peer saturation,
/// relief holding, overtaking prevention, and release wakeup.
#[test]
fn test_inbound_capacity_refines_the_model_ledger() {
    const MIB: usize = 1024 * 1024;
    assert_eq!(INBOUND_MODEL_RESERVED_BYTES_PER_LANE, MIB);
    assert_eq!(INBOUND_MODEL_PEER_BYTES, 128 * MIB);
    assert_eq!(INBOUND_MODEL_MAILBOX_BYTES, 256 * MIB);

    // Production-scaled isomorphic counterpart to `WORKLOAD`:
    // - lanes 2..4 stay idle, holding back `3 MiB` of unmet reservations, leaving `253 MiB` for
    //   lanes 0 (`DhtControl`) and 1 (`Storage`), each with a `1 MiB` fixed reservation;
    // - each peer has `128 MiB`.
    let prod_requests: [(u8, u8, usize); WORKLOAD.len()] = [
        (1, 1, 127 * MIB),
        (0, 1, 2 * MIB),
        (0, 0, 126 * MIB),
        (0, 0, MIB),
        (1, 0, MIB),
    ];
    let to_did = |peer: u8| Some(Did::from(u32::from(peer) + 1));
    let to_lane = |lane: u8| match lane {
        0 => InboundLane::DhtControl,
        _ => InboundLane::Storage,
    };
    let to_inbound_req = |index: usize| {
        let (peer, lane, bytes) = prod_requests[index];
        InboundRequest::bytes_for_test(to_did(peer), to_lane(lane), bytes)
    };
    let map_resource = |res: Resource| match res {
        Resource::Stream(peer, lane) => InboundResource::Stream(to_did(peer), to_lane(lane), None),
        Resource::Peer(peer) => InboundResource::Peer(to_did(peer)),
        Resource::Lane(lane) => InboundResource::Lane(to_lane(lane)),
        Resource::Shared => InboundResource::Shared,
    };

    // 1. Exhaustive subset differential check over all 2^5 combinations of active requests:
    //    `stream`, `footprint`, `reliefs`, and `try_admit` on `Arc<InboundCapacity>` must match
    //    the exact mathematical specification of `Ledger` scaled to production byte capacities.
    let other_reserved = (INBOUND_MODEL_LANES.saturating_sub(2)) * MIB;
    let effective_cap = INBOUND_MODEL_MAILBOX_BYTES - other_reserved;
    for mask in 0usize..(1 << prod_requests.len()) {
        let capacity = Arc::new(InboundCapacity::new());
        let mut held_permits: Vec<InboundCapacityPermit> = Vec::new();
        let mut lane_bytes = [0usize; 2];
        let mut peer_bytes = [0usize; 2];
        let mut feasible = true;
        for (idx, &(peer, lane, bytes)) in prod_requests.iter().enumerate() {
            if mask & (1 << idx) == 0 {
                continue;
            }
            let other_lane = 1 - usize::from(lane);
            let held_back = MIB.saturating_sub(lane_bytes[other_lane]);
            let peer_fits = peer_bytes[usize::from(peer)] + bytes <= INBOUND_MODEL_PEER_BYTES;
            let mailbox_fits = lane_bytes[0] + lane_bytes[1] + bytes <= effective_cap - held_back;
            let req = to_inbound_req(idx);
            match capacity.try_admit(&req) {
                Ok(permit) => {
                    assert!(
                        peer_fits && mailbox_fits,
                        "production admitted infeasible subset mask={mask:#b} at idx={idx}"
                    );
                    lane_bytes[usize::from(lane)] += bytes;
                    peer_bytes[usize::from(peer)] += bytes;
                    held_permits.push(permit);
                }
                Err(_) => {
                    assert!(
                        !(peer_fits && mailbox_fits),
                        "production refused feasible subset mask={mask:#b} at idx={idx}"
                    );
                    feasible = false;
                    break;
                }
            }
        }
        if !feasible {
            continue;
        }
        for (idx, &(peer, lane, bytes)) in prod_requests.iter().enumerate() {
            let req = to_inbound_req(idx);
            assert_eq!(
                capacity.stream(&req),
                map_resource(Resource::Stream(peer, lane))
            );
            let covers = lane_bytes[usize::from(lane)] + bytes <= MIB;
            let mut expected_footprint = vec![
                map_resource(Resource::Peer(peer)),
                map_resource(Resource::Lane(lane)),
            ];
            if !covers {
                expected_footprint.push(InboundResource::Shared);
            }
            assert_eq!(
                capacity.footprint(&req).collect::<Vec<_>>(),
                expected_footprint,
                "footprint drift at mask={mask:#b}, idx={idx}"
            );
            let expected_reliefs = if !covers && bytes <= MIB {
                vec![(map_resource(Resource::Lane(lane)), vec![
                    InboundResource::Shared,
                ])]
            } else {
                Vec::new()
            };
            assert_eq!(
                capacity.reliefs(&req),
                expected_reliefs,
                "reliefs drift at mask={mask:#b}, idx={idx}"
            );
            let other_lane = 1 - usize::from(lane);
            let held_back = MIB.saturating_sub(lane_bytes[other_lane]);
            let peer_fits = peer_bytes[usize::from(peer)] + bytes <= INBOUND_MODEL_PEER_BYTES;
            let mailbox_fits = lane_bytes[0] + lane_bytes[1] + bytes <= effective_cap - held_back;
            match capacity.try_admit(&req) {
                Ok(permit) => {
                    assert!(
                        peer_fits && mailbox_fits,
                        "try_admit drift at mask={mask:#b}, idx={idx}"
                    );
                    drop(permit);
                }
                Err(refusing) => {
                    let expected_refusal = if !peer_fits {
                        vec![map_resource(Resource::Peer(peer))]
                    } else {
                        assert!(!mailbox_fits);
                        vec![map_resource(Resource::Lane(lane)), InboundResource::Shared]
                    };
                    assert_eq!(
                        refusing.collect::<Vec<_>>(),
                        expected_refusal,
                        "refusal drift at mask={mask:#b}, idx={idx}"
                    );
                }
            }
        }
    }

    // 2. End-to-end `Waitlist` lockstep replay on `Arc<InboundCapacity>`:
    //    Hold `252 MiB` across two peers on `Storage` (`126 MiB + 126 MiB`), leaving only `1 MiB`
    //    (`DhtControl`'s fixed reservation) out of the `253 MiB` available outside lanes 2..4, and
    //    hold that `1 MiB` on `DhtControl` with `holder_ctrl`. Then queue request 3 (`1 MiB` on
    //    `DhtControl`), request 4 (`1 MiB` on `DhtControl`), and request 1 (`2 MiB` borrower on
    //    `Storage`), and verify step-by-step release ordering!
    let capacity = Arc::new(InboundCapacity::new());
    let bg_a = capacity
        .try_admit(&InboundRequest::bytes_for_test(
            Some(Did::from(10_u32)),
            InboundLane::Storage,
            126 * MIB,
        ))
        .expect("background storage borrower A");
    let bg_b = capacity
        .try_admit(&InboundRequest::bytes_for_test(
            Some(Did::from(11_u32)),
            InboundLane::Storage,
            126 * MIB,
        ))
        .expect("background storage borrower B");
    let holder_ctrl = capacity
        .try_admit(&InboundRequest::bytes_for_test(
            Some(Did::from(12_u32)),
            InboundLane::DhtControl,
            MIB,
        ))
        .expect("initial holder of DhtControl reservation");

    let mut prod_waitlist: Waitlist<(usize, InboundRequest), InboundResource> = Waitlist::default();
    let mut prod_permits: BTreeMap<usize, InboundCapacityPermit> = BTreeMap::new();

    struct IndexedInboundLedger(Arc<InboundCapacity>);
    impl AdmissionLedger for IndexedInboundLedger {
        type Grant = (usize, InboundCapacityPermit);
        type Request = (usize, InboundRequest);
        type Resource = InboundResource;
        type Resources = Vec<InboundResource>;

        fn stream(&self, (_, req): &Self::Request) -> InboundResource {
            self.0.stream(req)
        }
        fn footprint(&self, (_, req): &Self::Request) -> Vec<InboundResource> {
            self.0.footprint(req).collect()
        }
        fn reliefs(
            &self,
            (_, req): &Self::Request,
        ) -> Vec<(InboundResource, Vec<InboundResource>)> {
            self.0.reliefs(req)
        }
        fn try_admit(
            &self,
            (idx, req): &Self::Request,
        ) -> Result<Self::Grant, Vec<InboundResource>> {
            self.0
                .try_admit(req)
                .map(|permit| (*idx, permit))
                .map_err(|res| res.collect())
        }
    }

    let prod_ledger = IndexedInboundLedger(Arc::clone(&capacity));
    for idx in [3usize, 4, 1] {
        let (_, served) = prod_waitlist.arrive(&prod_ledger, (idx, to_inbound_req(idx)));
        for (_, (granted_idx, permit)) in served.granted {
            prod_permits.insert(granted_idx, permit);
        }
    }
    assert!(
        prod_permits.is_empty(),
        "mailbox is full so 3, 4, 1 all wait"
    );
    assert_eq!(prod_waitlist.len(), 3, "three requests waiting");

    // Release `holder_ctrl` (1 MiB on DhtControl): Request 3 (1 MiB on DhtControl) takes the 1 MiB
    // reservation first while `bg_a` + `bg_b` still hold 252 MiB on Storage; Request 4 waits!
    drop(holder_ctrl);
    let served = prod_waitlist.release(&prod_ledger, [
        InboundResource::Peer(Some(Did::from(12_u32))),
        InboundResource::Lane(InboundLane::DhtControl),
        InboundResource::Shared,
    ]);
    for (_, (granted_idx, permit)) in served.granted {
        prod_permits.insert(granted_idx, permit);
    }
    assert!(
        prod_permits.contains_key(&3)
            && !prod_permits.contains_key(&4)
            && !prod_permits.contains_key(&1),
        "request 3 takes DhtControl reservation first; request 4 and borrower 1 wait"
    );

    // Release 3: Request 4 (Peer 1's 1 MiB on DhtControl) now takes DhtControl's reservation!
    prod_permits.remove(&3);
    let served = prod_waitlist.release(&prod_ledger, [
        InboundResource::Peer(to_did(0)),
        InboundResource::Lane(InboundLane::DhtControl),
        InboundResource::Shared,
    ]);
    for (_, (granted_idx, permit)) in served.granted {
        prod_permits.insert(granted_idx, permit);
    }
    assert!(prod_permits.contains_key(&4) && !prod_permits.contains_key(&1));

    // Release `bg_a` (126 MiB on Storage): Request 1 (Peer 0's 2 MiB borrower on Storage) is now
    // admitted even while Request 4 still holds DhtControl!
    drop(bg_a);
    let served = prod_waitlist.release(&prod_ledger, [
        InboundResource::Peer(Some(Did::from(10_u32))),
        InboundResource::Lane(InboundLane::Storage),
        InboundResource::Shared,
    ]);
    for (_, (granted_idx, permit)) in served.granted {
        prod_permits.insert(granted_idx, permit);
    }
    assert!(prod_permits.contains_key(&1));
    assert_eq!(prod_waitlist.len(), 0);
    drop((bg_b, prod_permits));
}
