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
//! arrival, release and cancellation runs one pass of the discipline under check.
//!
//! Properties:
//!
//! - `Always` no overtaking: within a pass, no request is granted whose footprint meets the
//!   refusal of an earlier request of the same pass;
//! - `Always` stream order: no granted request has an earlier request of its stream still
//!   waiting ungranted;
//! - `Always` independence (no lost wake-up): no waiter is left ungranted that the ledger
//!   would admit, unless an earlier ungranted waiter shares its stream or has a refusal, judged
//!   on the ledger on its own, that meets its footprint. Each earlier waiter is judged
//!   independently, never through the waiters a pass skipped, so it is not a pass restated;
//! - `Eventually` resolved: every path ends with every request released or cancelled;
//! - `Sometimes` a later request passes a waiting one, a waiter is refused, and all are resolved,
//!   so the safety properties are not vacuous.
//!
//! Each property is shown not to be vacuous by a mutant discipline that the checker refutes:
//! one first-in-first-out queue (independence), admission that ignores refusals (no overtaking), a
//! skipped request that does not hold its stream (stream order), and a cancellation that runs
//! no pass (eventually resolved).

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::collections::VecDeque;

use stateright::Checker;
use stateright::Model;
use stateright::Property;

use super::resource_order::serve;
use super::resource_order::AdmissionLedger;
use super::resource_order::Waiting;

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

    /// Release `request`'s units.
    fn release(&mut self, request: &Request) {
        self.lanes[usize::from(request.lane)] -= request.size;
        self.peers[usize::from(request.peer)] -= request.size;
    }
}

/// One admission attempt of a pass: the request, its footprint, and the refusal, if refused.
type Attempt = (Request, Vec<Resource>, Option<Vec<Resource>>);

/// The abstract ledger as an [`AdmissionLedger`], recording every attempt of a pass.
struct ModelLedger {
    /// The admitted units.
    ledger: RefCell<Ledger>,
    /// The attempts of the pass, in order.
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

    fn try_admit(&self, request: &Request) -> Result<u8, Vec<Resource>> {
        let footprint = self.footprint(request);
        let outcome = self.ledger.borrow_mut().admit(request);
        self.attempts
            .borrow_mut()
            .push((*request, footprint, outcome.clone().err()));
        outcome.map(|()| request.id)
    }
}

/// A discipline of one pass: production, or a mutant that must be refuted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Discipline {
    /// [`serve`] itself.
    Production,
    /// One first-in-first-out queue: the pass stops at the first refusal.
    Fifo,
    /// Every waiter is attempted, whatever earlier waiters were refused by.
    IgnoreRefusals,
    /// A skipped waiter does not hold its stream.
    SkipFreesStream,
    /// [`serve`], but a cancellation runs no pass.
    CancelWithoutPass,
}

/// The state of the model.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct State {
    /// The admitted units.
    ledger: Ledger,
    /// The waiting requests in arrival order, with whether a pass granted them.
    queue: VecDeque<(u8, bool)>,
    /// The arrival sequence of each request, once arrived.
    arrival: [Option<u8>; WORKLOAD.len()],
    /// The requests holding a claimed grant.
    held: BTreeSet<u8>,
    /// The requests released or cancelled.
    resolved: BTreeSet<u8>,
    /// Cancellations still allowed.
    cancels: u8,
    /// Whether some pass granted a request past an earlier refusal it draws on.
    overtaken: bool,
}

impl State {
    /// Whether every request is released or cancelled.
    fn done(&self) -> bool {
        self.resolved.len() == WORKLOAD.len()
    }

    /// Whether a granted request arrived after a request still waiting ungranted that is
    /// `related` to it.
    fn granted_past_a_waiter(&self, related: impl Fn(&Request, &Request) -> bool) -> bool {
        let mut granted = self
            .queue
            .iter()
            .filter(|(_, granted)| *granted)
            .map(|(id, _)| *id)
            .chain(self.held.iter().copied());
        granted.any(|later| {
            self.queue.iter().any(|(earlier, granted)| {
                !granted
                    && related(&request(*earlier), &request(later))
                    && self.arrival[usize::from(*earlier)] < self.arrival[usize::from(later)]
            })
        })
    }

    /// Whether some ungranted waiter is admissible on the ledger as it stands with nothing to
    /// justify its wait: no earlier ungranted waiter of its stream, and none whose refusal on
    /// that ledger, judged on its own, meets the waiter's footprint.
    ///
    /// Each earlier waiter is judged independently against the ledger, not by replaying a
    /// pass: a waiter a pass skipped still counts by its own refusal, and nothing is inherited
    /// transitively from the waiters it skipped.
    fn admissible_waiter_left(&self) -> bool {
        let ungranted = self
            .queue
            .iter()
            .filter(|(_, granted)| !granted)
            .map(|(id, _)| request(*id))
            .collect::<Vec<_>>();
        let refusal = |waiter: &Request| {
            let mut ledger = self.ledger;
            ledger.admit(waiter).err().unwrap_or_default()
        };
        ungranted.iter().enumerate().any(|(position, waiter)| {
            let mut ledger = self.ledger;
            let footprint = ModelLedger::footprint_of(&self.ledger, waiter);
            let justified = ungranted.iter().take(position).any(|earlier| {
                (earlier.peer, earlier.lane) == (waiter.peer, waiter.lane)
                    || refusal(earlier)
                        .iter()
                        .any(|resource| footprint.contains(resource))
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

/// One mutant pass over `waiting`: the stand-in for the production pass that `discipline`
/// makes; returns how many requests it granted.
fn mutant_pass(
    ledger: &ModelLedger,
    waiting: &mut [Waiting<ModelLedger>],
    discipline: Discipline,
) -> usize {
    let mut exhausted = BTreeSet::new();
    let mut granted = 0;
    for waiter in waiting.iter_mut().filter(|waiter| waiter.grant.is_none()) {
        let stream = ledger.stream(&waiter.request);
        let blocked = match discipline {
            Discipline::Fifo => !exhausted.is_empty(),
            Discipline::IgnoreRefusals => false,
            _ => {
                exhausted.contains(&stream)
                    || ledger
                        .footprint(&waiter.request)
                        .iter()
                        .any(|resource| exhausted.contains(resource))
            }
        };
        if blocked {
            continue;
        }
        match ledger.try_admit(&waiter.request) {
            Ok(grant) => {
                waiter.grant = Some(grant);
                granted += 1;
            }
            Err(refused) => {
                exhausted.extend(refused);
                exhausted.insert(stream);
            }
        }
    }
    granted
}

/// Whether some pass of `attempts` granted a request whose footprint meets the refusal of an
/// earlier request of the same pass. A pass visits the queue in arrival order, so a new pass
/// begins where an attempt's position does not exceed the previous one's.
fn overtakes(attempts: &[Attempt], position: impl Fn(u8) -> usize) -> bool {
    let mut refusals: Vec<&Vec<Resource>> = Vec::new();
    let mut last = None;
    for (request, footprint, refused) in attempts {
        let at = position(request.id);
        if last.is_some_and(|last| at <= last) {
            refusals.clear();
        }
        last = Some(at);
        match refused {
            Some(refused) => refusals.push(refused),
            None => {
                let met = refusals
                    .iter()
                    .any(|refused| refused.iter().any(|resource| footprint.contains(resource)));
                if met {
                    return true;
                }
            }
        }
    }
    false
}

/// Serve `state` under `discipline`, recording an overtaking.
fn pass(state: &mut State, discipline: Discipline) {
    let ledger = ModelLedger {
        ledger: RefCell::new(state.ledger),
        attempts: RefCell::new(Vec::new()),
    };
    let mut waiting: Vec<Waiting<ModelLedger>> = state
        .queue
        .iter()
        .map(|(id, granted)| Waiting {
            request: request(*id),
            grant: granted.then_some(*id),
        })
        .collect();
    match discipline {
        Discipline::Production | Discipline::CancelWithoutPass => {
            serve(&ledger, waiting.as_mut_slice(), |waiter| waiter);
        }
        Discipline::Fifo | Discipline::IgnoreRefusals | Discipline::SkipFreesStream => {
            while mutant_pass(&ledger, waiting.as_mut_slice(), discipline) > 0 {}
        }
    }
    let queue = state.queue.clone();
    let position = |id: u8| {
        queue
            .iter()
            .position(|(queued, _)| *queued == id)
            .unwrap_or(usize::MAX)
    };
    state.overtaken |= overtakes(&ledger.attempts.borrow(), position);
    state.ledger = ledger.ledger.into_inner();
    for ((_, granted), waiter) in state.queue.iter_mut().zip(waiting) {
        *granted = waiter.grant.is_some();
    }
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
        vec![State {
            ledger: Ledger::default(),
            queue: VecDeque::new(),
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
        for (id, granted) in state.queue.iter().copied() {
            if granted {
                actions.push(Action::Claim(id));
            }
            if state.cancels > 0 {
                actions.push(Action::Cancel(id));
            }
        }
        actions.extend(state.held.iter().copied().map(Action::Release));
    }

    fn next_state(&self, last: &State, action: Action) -> Option<State> {
        let mut state = last.clone();
        match action {
            Action::Arrive(id) => {
                let sequence = u8::try_from(state.arrival.iter().flatten().count()).ok()?;
                state.arrival[usize::from(id)] = Some(sequence);
                state.queue.push_back((id, false));
                pass(&mut state, self.discipline);
            }
            Action::Claim(id) => {
                let position = state.queue.iter().position(|entry| *entry == (id, true))?;
                state.queue.remove(position);
                state.held.insert(id);
            }
            Action::Release(id) => {
                state.held.remove(&id).then_some(())?;
                state.ledger.release(&request(id));
                state.resolved.insert(id);
                pass(&mut state, self.discipline);
            }
            Action::Cancel(id) => {
                let position = state.queue.iter().position(|(queued, _)| *queued == id)?;
                let (_, granted) = state.queue.remove(position)?;
                if granted {
                    state.ledger.release(&request(id));
                }
                state.cancels -= 1;
                state.resolved.insert(id);
                if self.discipline != Discipline::CancelWithoutPass {
                    pass(&mut state, self.discipline);
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
                state.queue.iter().any(|(_, granted)| !granted)
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
    for (discipline, law) in [
        (Discipline::Fifo, "independence"),
        (Discipline::IgnoreRefusals, "no overtaking"),
        (Discipline::SkipFreesStream, "stream order"),
        (Discipline::CancelWithoutPass, "resolved"),
    ] {
        assert!(
            refuted(discipline).contains(law),
            "{discipline:?} must be refuted by `{law}`"
        );
    }
}
