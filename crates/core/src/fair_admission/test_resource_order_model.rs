//! Model check of resource-ordered admission over every interleaving of a fixed workload.
//!
//! The ledger is an abstract node budget with one reserved lane, a shared pool and a budget
//! per peer, the shape of the inbound mailbox, whose budgets bound themselves (its gates are
//! the ledger's own, so they are not a property here); its footprint and refusals follow
//! `InboundCapacity`'s (`InboundResource`), which a change there must be mirrored into:
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
//! - `Eventually` resolved: every path ends with every request released or cancelled;
//! - `Sometimes` a later request passes a waiting one, a waiter is refused, and all are resolved,
//!   so the safety properties are not vacuous.
//!
//! Each property is shown not to be vacuous by a mutant discipline that the checker refutes:
//! one first-in-first-out queue (independence), admission that ignores refusals (no overtaking), a
//! blocked request that does not hold its stream (stream order), a withdrawal that scans nothing
//! (eventually resolved), a release that scans nothing (independence), and a request that holds
//! its lane while its own peer's budget blocks it (independence: another peer waits behind that
//! budget).

use std::cell::RefCell;
use std::collections::BTreeSet;

use stateright::Checker;
use stateright::Model;
use stateright::Property;

use super::resource_order::AdmissionLedger;
use super::waitlist::Arrival;
use super::waitlist::Mutant;
use super::waitlist::Served;
use super::waitlist::Waitlist;

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
    /// The requests holding a claimed grant.
    held: BTreeSet<u8>,
    /// The requests released or cancelled.
    resolved: BTreeSet<u8>,
    /// Cancellations still allowed.
    cancels: u8,
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
    /// Whether to check liveness alone (the `eventually` property).
    liveness: bool,
}

impl Model for AdmissionModel {
    type Action = Action;
    type State = State;

    fn init_states(&self) -> Vec<State> {
        let waitlist = match self.discipline {
            Discipline::Production => Waitlist::default(),
            Discipline::Broken(mutant) => Waitlist::mutant(mutant),
        };
        vec![State {
            ledger: Ledger::default(),
            waitlist,
            granted: BTreeSet::new(),
            arrival: [None; WORKLOAD.len()],
            held: BTreeSet::new(),
            resolved: BTreeSet::new(),
            cancels: 1,
            overtaken: false,
        }]
    }

    fn actions(&self, state: &State, actions: &mut Vec<Action>) {
        for candidate in WORKLOAD
            .iter()
            .filter(|candidate| state.arrival[usize::from(candidate.id)].is_none())
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
                let mut arrived = None;
                event(&mut state, |waitlist, ledger| {
                    let (arrival, served) = waitlist.arrive(ledger, request(id));
                    arrived = Some(arrival);
                    served
                });
                state.arrival[usize::from(id)] = arrived;
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
        vec![
            Property::always("no overtaking", |_, state: &State| !state.overtaken),
            Property::always("stream order", |_, state: &State| !state.stream_reordered()),
            Property::always("independence", |_, state: &State| {
                !state.admissible_waiter_left()
            }),
            Property::sometimes(
                "a later request passes a waiting one",
                |_, state: &State| state.passed(),
            ),
            Property::sometimes("a waiter is refused", |_, state: &State| {
                state.waitlist.len() > 0
            }),
            Property::sometimes("all resolved", |_, state: &State| state.done()),
        ]
    }
}

/// Check `discipline`'s safety and its liveness, returning the names of refuted properties.
fn refuted(discipline: Discipline) -> BTreeSet<&'static str> {
    let mut refuted = BTreeSet::new();
    for liveness in [false, true] {
        let checker = AdmissionModel {
            discipline,
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

/// The production pass satisfies every law over every interleaving of the workload.
#[test]
fn test_resource_order_satisfies_its_laws() {
    assert_eq!(refuted(Discipline::Production), BTreeSet::new());
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
}
