use super::*;
use crate::ecc::SecretKey;

#[test]
fn test_event_disposition_is_defined_by_the_active_generation_only() -> Result<()> {
    let peer = SecretKey::random().address().into();
    let source = PendingConnectionAttempt {
        peer,
        generation: 1,
    };
    let replacement = PendingConnectionAttempt {
        peer,
        generation: 2,
    };

    assert_eq!(
        event_disposition(None, source),
        ConnectionEventDisposition::Deliver
    );
    assert_eq!(
        event_disposition(
            Some(PeerConnectionLifecycle::Pending {
                attempt: replacement,
                started_at_ms: 0,
            }),
            source,
        ),
        ConnectionEventDisposition::Deliver
    );
    assert_eq!(
        event_disposition(
            Some(PeerConnectionLifecycle::Admitting {
                attempt: replacement,
                started_at_ms: 0,
            }),
            source,
        ),
        ConnectionEventDisposition::Deliver
    );
    assert_eq!(
        event_disposition(Some(PeerConnectionLifecycle::Active(replacement)), source,),
        ConnectionEventDisposition::Suppress {
            active: replacement
        }
    );
    assert_eq!(
        event_disposition(Some(PeerConnectionLifecycle::Active(source)), source),
        ConnectionEventDisposition::Deliver
    );
    Ok(())
}

#[test]
fn test_finger_candidate_admission_is_total_over_lifecycle_and_routability() {
    let peer = SecretKey::random().address().into();
    let attempt = PendingConnectionAttempt {
        peer,
        generation: 1,
    };
    let cases = [
        (None, false, FingerCandidateAdmission::Missing),
        (None, true, FingerCandidateAdmission::Missing),
        (
            Some(PeerConnectionLifecycle::Pending {
                attempt,
                started_at_ms: 0,
            }),
            false,
            FingerCandidateAdmission::Queue(attempt),
        ),
        (
            Some(PeerConnectionLifecycle::Pending {
                attempt,
                started_at_ms: 0,
            }),
            true,
            FingerCandidateAdmission::Queue(attempt),
        ),
        (
            Some(PeerConnectionLifecycle::Admitting {
                attempt,
                started_at_ms: 0,
            }),
            false,
            FingerCandidateAdmission::Queue(attempt),
        ),
        (
            Some(PeerConnectionLifecycle::Admitting {
                attempt,
                started_at_ms: 0,
            }),
            true,
            FingerCandidateAdmission::Queue(attempt),
        ),
        (
            Some(PeerConnectionLifecycle::Active(attempt)),
            false,
            FingerCandidateAdmission::Unroutable,
        ),
        (
            Some(PeerConnectionLifecycle::Active(attempt)),
            true,
            FingerCandidateAdmission::Apply,
        ),
    ];

    for (lifecycle, is_routable, expected) in cases {
        assert_eq!(finger_candidate_admission(lifecycle, is_routable), expected);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PeerLifecycle {
    Absent,
    Pending(u64),
    Admitting(u64),
    Active(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CallbackGeneration {
    Current,
    Previous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecycleAction {
    Reserve,
    BeginAdmission(CallbackGeneration),
    CommitAdmission(CallbackGeneration),
    Terminal(CallbackGeneration),
    Timeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LifecycleView {
    peer: PeerLifecycle,
    dht_member: bool,
    transport_slot: bool,
    generation_exhausted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LifecycleModel {
    peer: PeerLifecycle,
    next_generation: u64,
    previous_generation: Option<u64>,
    generation_exhausted: bool,
}

impl Default for LifecycleModel {
    fn default() -> Self {
        Self {
            peer: PeerLifecycle::Absent,
            next_generation: 0,
            previous_generation: None,
            generation_exhausted: false,
        }
    }
}

impl LifecycleModel {
    fn apply(mut self, action: LifecycleAction) -> Self {
        match action {
            LifecycleAction::Reserve => self.reserve(),
            LifecycleAction::BeginAdmission(callback) => self.begin_admission(callback),
            LifecycleAction::CommitAdmission(callback) => self.commit_admission(callback),
            LifecycleAction::Terminal(callback) => self.terminal(callback),
            LifecycleAction::Timeout => self.timeout(),
        }
        self
    }

    fn reserve(&mut self) {
        if !matches!(self.peer, PeerLifecycle::Absent) {
            return;
        }
        let Some(next_generation) = self.next_generation.checked_add(1) else {
            self.generation_exhausted = true;
            return;
        };
        self.next_generation = next_generation;
        self.peer = PeerLifecycle::Pending(next_generation);
    }

    fn begin_admission(&mut self, callback: CallbackGeneration) {
        let Some(callback_generation) = self.callback_generation(callback) else {
            return;
        };
        if self.peer == PeerLifecycle::Pending(callback_generation) {
            self.peer = PeerLifecycle::Admitting(callback_generation);
        }
    }

    fn commit_admission(&mut self, callback: CallbackGeneration) {
        let Some(callback_generation) = self.callback_generation(callback) else {
            return;
        };
        if self.peer == PeerLifecycle::Admitting(callback_generation) {
            self.peer = PeerLifecycle::Active(callback_generation);
        }
    }

    fn terminal(&mut self, callback: CallbackGeneration) {
        let Some(callback_generation) = self.callback_generation(callback) else {
            return;
        };
        if self.generation_matches_live_slot(callback_generation) {
            self.previous_generation = Some(callback_generation);
            self.peer = PeerLifecycle::Absent;
        }
    }

    fn timeout(&mut self) {
        match self.peer {
            PeerLifecycle::Pending(generation) | PeerLifecycle::Admitting(generation) => {
                self.previous_generation = Some(generation);
                self.peer = PeerLifecycle::Absent;
            }
            PeerLifecycle::Absent | PeerLifecycle::Active(_) => {}
        }
    }

    fn callback_generation(self, callback: CallbackGeneration) -> Option<u64> {
        match callback {
            CallbackGeneration::Current => self.current_generation(),
            CallbackGeneration::Previous => self.previous_generation,
        }
    }

    fn current_generation(self) -> Option<u64> {
        match self.peer {
            PeerLifecycle::Absent => None,
            PeerLifecycle::Pending(generation)
            | PeerLifecycle::Admitting(generation)
            | PeerLifecycle::Active(generation) => Some(generation),
        }
    }

    fn generation_matches_live_slot(self, generation: u64) -> bool {
        matches!(
            self.peer,
            PeerLifecycle::Pending(current)
                | PeerLifecycle::Admitting(current)
                | PeerLifecycle::Active(current)
                if current == generation
        )
    }

    fn view(self) -> LifecycleView {
        LifecycleView {
            peer: self.peer,
            dht_member: matches!(self.peer, PeerLifecycle::Active(_)),
            transport_slot: matches!(
                self.peer,
                PeerLifecycle::Pending(_) | PeerLifecycle::Admitting(_) | PeerLifecycle::Active(_)
            ),
            generation_exhausted: self.generation_exhausted,
        }
    }
}

#[derive(Clone)]
struct LifecycleImplementation {
    peer: Did,
    lifecycles: ConnectionLifecycleRegistry<1>,
    previous_attempt: Option<PendingConnectionAttempt>,
    dht_member: bool,
    transport_slot: bool,
    generation_exhausted: bool,
    now_ms: i64,
}

impl LifecycleImplementation {
    fn new(peer: Did) -> Self {
        Self {
            peer,
            lifecycles: ConnectionLifecycleRegistry::new(1),
            previous_attempt: None,
            dht_member: false,
            transport_slot: false,
            generation_exhausted: false,
            now_ms: 0,
        }
    }

    fn with_next_generation(peer: Did, next_generation: u64) -> Self {
        let mut this = Self::new(peer);
        this.lifecycles
            .set_next_generation_for_test(next_generation);
        this
    }

    fn apply(mut self, action: LifecycleAction) -> Self {
        match action {
            LifecycleAction::Reserve => self.reserve(),
            LifecycleAction::BeginAdmission(callback) => self.begin_admission(callback),
            LifecycleAction::CommitAdmission(callback) => self.commit_admission(callback),
            LifecycleAction::Terminal(callback) => self.terminal(callback),
            LifecycleAction::Timeout => self.timeout(),
        }
        self
    }

    fn reserve(&mut self) {
        match self.lifecycles.reserve(self.peer, self.now_ms) {
            Ok(_) => {
                self.transport_slot = true;
                self.dht_member = false;
                self.now_ms += 1;
            }
            Err(Error::AlreadyConnected) => {}
            Err(Error::PendingConnectionGenerationExhausted) => {
                self.generation_exhausted = true;
            }
            Err(error) => panic!("unexpected pending reserve error: {error:?}"),
        }
    }

    fn begin_admission(&mut self, callback: CallbackGeneration) {
        let Some(attempt) = self.callback_attempt(callback) else {
            return;
        };
        if !self.lifecycles.begin_admission(attempt) {
            return;
        }
        self.transport_slot = true;
    }

    fn commit_admission(&mut self, callback: CallbackGeneration) {
        let Some(attempt) = self.callback_attempt(callback) else {
            return;
        };
        let Some(admitting) = self.lifecycles.admitting_connection(attempt) else {
            return;
        };
        admitting.activate();
        self.dht_member = true;
        self.transport_slot = true;
    }

    fn terminal(&mut self, callback: CallbackGeneration) {
        let Some(attempt) = self.callback_attempt(callback) else {
            return;
        };
        let removed_pending = self.lifecycles.remove_unadmitted(attempt);
        let removed_active = self.lifecycles.remove_active(attempt);
        if !removed_pending && !removed_active {
            return;
        }
        self.previous_attempt = Some(attempt);
        self.dht_member = false;
        self.transport_slot = false;
    }

    fn timeout(&mut self) {
        let expired = self
            .lifecycles
            .expire(self.now_ms + PENDING_CONNECTION_TIMEOUT_MS);
        let Some(expired) = expired.into_iter().next() else {
            return;
        };
        self.previous_attempt = Some(expired.attempt);
        self.dht_member = false;
        self.transport_slot = false;
    }

    fn callback_attempt(&self, callback: CallbackGeneration) -> Option<PendingConnectionAttempt> {
        match callback {
            CallbackGeneration::Current => self
                .lifecycles
                .state(self.peer)
                .map(|state| state.attempt()),
            CallbackGeneration::Previous => self.previous_attempt,
        }
    }

    fn view(&self) -> LifecycleView {
        let peer = match self.lifecycles.state(self.peer) {
            Some(PeerConnectionLifecycle::Pending { attempt, .. }) => {
                PeerLifecycle::Pending(attempt.generation)
            }
            Some(PeerConnectionLifecycle::Admitting { attempt, .. }) => {
                PeerLifecycle::Admitting(attempt.generation)
            }
            Some(PeerConnectionLifecycle::Active(attempt)) => {
                PeerLifecycle::Active(attempt.generation)
            }
            None => PeerLifecycle::Absent,
        };
        LifecycleView {
            peer,
            dht_member: self.dht_member,
            transport_slot: self.transport_slot,
            generation_exhausted: self.generation_exhausted,
        }
    }
}

impl LifecycleView {
    fn assert_invariants(self) {
        // Invariant: unfinished admission is a transport slot, never a Chord member.
        if matches!(
            self.peer,
            PeerLifecycle::Pending(_) | PeerLifecycle::Admitting(_)
        ) {
            assert!(
                !self.dht_member,
                "unadmitted peers must never be routable: {self:?}"
            );
        }
        assert_eq!(
            self.dht_member,
            matches!(self.peer, PeerLifecycle::Active(_)),
            "only admitted active peers may appear in DHT membership: {self:?}"
        );
        assert_eq!(
            self.transport_slot,
            matches!(
                self.peer,
                PeerLifecycle::Pending(_) | PeerLifecycle::Admitting(_) | PeerLifecycle::Active(_)
            ),
            "terminal and expiry transitions must remove the transport slot: {self:?}"
        );
    }
}

#[test]
fn test_pending_admission_model_preserves_generation_and_routing_invariants() {
    const MAX_DEPTH: usize = 6;
    let actions = [
        LifecycleAction::Reserve,
        LifecycleAction::BeginAdmission(CallbackGeneration::Current),
        LifecycleAction::BeginAdmission(CallbackGeneration::Previous),
        LifecycleAction::CommitAdmission(CallbackGeneration::Current),
        LifecycleAction::CommitAdmission(CallbackGeneration::Previous),
        LifecycleAction::Terminal(CallbackGeneration::Current),
        LifecycleAction::Terminal(CallbackGeneration::Previous),
        LifecycleAction::Timeout,
    ];
    let peer = SecretKey::random().address().into();

    explore(
        LifecycleModel::default(),
        LifecycleImplementation::new(peer),
        &actions,
        MAX_DEPTH,
    );
}

#[test]
fn test_pending_admission_model_and_registry_reject_generation_exhaustion_without_reuse() {
    let peer = SecretKey::random().address().into();
    let model = LifecycleModel {
        next_generation: u64::MAX,
        ..LifecycleModel::default()
    }
    .apply(LifecycleAction::Reserve);
    let implementation = LifecycleImplementation::with_next_generation(peer, u64::MAX)
        .apply(LifecycleAction::Reserve);

    assert_eq!(model.view(), implementation.view());
    assert_eq!(model.view().peer, PeerLifecycle::Absent);
    assert!(model.view().generation_exhausted);
    model.view().assert_invariants();
}

fn explore(
    model: LifecycleModel,
    implementation: LifecycleImplementation,
    actions: &[LifecycleAction],
    remaining_depth: usize,
) {
    assert_eq!(model.view(), implementation.view());
    model.view().assert_invariants();
    if remaining_depth == 0 {
        return;
    }
    for action in actions.iter().copied() {
        explore(
            model.apply(action),
            implementation.clone().apply(action),
            actions,
            remaining_depth - 1,
        );
    }
}
