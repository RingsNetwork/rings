use super::finger::finger_candidate_admission;
use super::finger::FingerCandidateAdmission;
use super::*;
use crate::ecc::SecretKey;

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
            Some(PeerSlotLifecycle::Pending {
                attempt,
                started_at_ms: 0,
            }),
            false,
            FingerCandidateAdmission::Queue(attempt),
        ),
        (
            Some(PeerSlotLifecycle::Pending {
                attempt,
                started_at_ms: 0,
            }),
            true,
            FingerCandidateAdmission::Queue(attempt),
        ),
        (
            Some(PeerSlotLifecycle::Admitting {
                attempt,
                started_at_ms: 0,
            }),
            false,
            FingerCandidateAdmission::Queue(attempt),
        ),
        (
            Some(PeerSlotLifecycle::Admitting {
                attempt,
                started_at_ms: 0,
            }),
            true,
            FingerCandidateAdmission::Queue(attempt),
        ),
        (
            Some(PeerSlotLifecycle::Active { attempt }),
            false,
            FingerCandidateAdmission::Unroutable,
        ),
        (
            Some(PeerSlotLifecycle::Active { attempt }),
            true,
            FingerCandidateAdmission::Apply,
        ),
    ];

    for (lifecycle, is_routable, expected) in cases {
        assert_eq!(finger_candidate_admission(lifecycle, is_routable), expected);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PeerLifecycle {
    #[default]
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

/// Per-peer phase model of the registry.
///
/// Scope: the transitions of one peer's generation. The cardinality bounds
/// (`LifecycleBounds`) range over the whole peer map and are outside this
/// model; the table tests in `tests/test_lifecycle.rs` witness them.
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
    lifecycles: ConnectionLifecycleRegistry,
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
            lifecycles: ConnectionLifecycleRegistry::new(LifecycleBounds::new(1, 1)),
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
            Some(PeerSlotLifecycle::Pending { attempt, .. }) => {
                PeerLifecycle::Pending(attempt.generation)
            }
            Some(PeerSlotLifecycle::Admitting { attempt, .. }) => {
                PeerLifecycle::Admitting(attempt.generation)
            }
            Some(PeerSlotLifecycle::Active { attempt }) => {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModelSignaling {
    Creating(OfferOwner),
    Sent(OfferOwner),
    SentContinuation(OfferOwner),
    Answered(OfferOwner),
    Answering(bool),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProductAction {
    ReserveOffering(OfferOwner),
    ReserveAnswering,
    MarkSent(CallbackGeneration),
    MarkContinuationSent(CallbackGeneration),
    MarkAnswered(CallbackGeneration),
    AwaitAnsweringContinuation(CallbackGeneration),
    ClaimAnsweringContinuation,
    CrossingOffer {
        carrier: OfferOwner,
        local_smaller: bool,
    },
    BeginAdmission(CallbackGeneration),
    CommitAdmission(CallbackGeneration),
    Terminal(CallbackGeneration),
    Timeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProductView {
    peer: PeerLifecycle,
    signaling: Option<ModelSignaling>,
    superseded_by_peer: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ProductModel {
    peer: PeerLifecycle,
    signaling: Option<ModelSignaling>,
    next_generation: u64,
    previous_generation: Option<u64>,
}

impl ProductModel {
    fn apply(mut self, action: ProductAction) -> Self {
        match action {
            ProductAction::ReserveOffering(owner) => {
                if matches!(self.peer, PeerLifecycle::Absent) {
                    if let Some(next) = self.next_generation.checked_add(1) {
                        self.next_generation = next;
                        self.peer = PeerLifecycle::Pending(next);
                        self.signaling = Some(ModelSignaling::Creating(owner));
                    }
                }
            }
            ProductAction::ReserveAnswering => {
                if matches!(self.peer, PeerLifecycle::Absent) {
                    if let Some(next) = self.next_generation.checked_add(1) {
                        self.next_generation = next;
                        self.peer = PeerLifecycle::Pending(next);
                        self.signaling = Some(ModelSignaling::Answering(false));
                    }
                }
            }
            ProductAction::MarkSent(cb) => {
                if let (
                    Some(gen),
                    PeerLifecycle::Pending(cur),
                    Some(ModelSignaling::Creating(owner)),
                ) = (self.callback_gen(cb), self.peer, self.signaling)
                {
                    if gen == cur {
                        self.signaling = Some(ModelSignaling::Sent(owner));
                    }
                }
            }
            ProductAction::MarkContinuationSent(cb) => {
                if let (Some(gen), PeerLifecycle::Pending(cur), Some(ModelSignaling::Sent(owner))) =
                    (self.callback_gen(cb), self.peer, self.signaling)
                {
                    if gen == cur {
                        self.signaling = Some(ModelSignaling::SentContinuation(owner));
                    }
                }
            }
            ProductAction::MarkAnswered(cb) => {
                if let (
                    Some(gen),
                    PeerLifecycle::Pending(cur),
                    Some(
                        ModelSignaling::Creating(owner)
                        | ModelSignaling::Sent(owner)
                        | ModelSignaling::SentContinuation(owner),
                    ),
                ) = (self.callback_gen(cb), self.peer, self.signaling)
                {
                    if gen == cur {
                        self.signaling = Some(ModelSignaling::Answered(owner));
                    }
                }
            }
            ProductAction::AwaitAnsweringContinuation(cb) => {
                if let (
                    Some(gen),
                    PeerLifecycle::Pending(cur),
                    Some(ModelSignaling::Answering(false)),
                ) = (self.callback_gen(cb), self.peer, self.signaling)
                {
                    if gen == cur {
                        self.signaling = Some(ModelSignaling::Answering(true));
                    }
                }
            }
            ProductAction::ClaimAnsweringContinuation => {
                if let (PeerLifecycle::Pending(_), Some(ModelSignaling::Answering(true))) =
                    (self.peer, self.signaling)
                {
                    self.signaling = Some(ModelSignaling::Answering(false));
                }
            }
            ProductAction::CrossingOffer {
                carrier,
                local_smaller,
            } => {
                if let (PeerLifecycle::Pending(cur), Some(sig)) = (self.peer, self.signaling) {
                    let yields = match sig {
                        ModelSignaling::Creating(_) => true,
                        ModelSignaling::Sent(OfferOwner::Overlay)
                            if carrier == OfferOwner::External =>
                        {
                            true
                        }
                        ModelSignaling::Sent(OfferOwner::External)
                            if carrier == OfferOwner::Overlay =>
                        {
                            false
                        }
                        ModelSignaling::Sent(_) => !local_smaller,
                        ModelSignaling::SentContinuation(_)
                        | ModelSignaling::Answered(_)
                        | ModelSignaling::Answering(_) => false,
                    };
                    if yields {
                        if let Some(next) = self.next_generation.checked_add(1) {
                            self.previous_generation = Some(cur);
                            self.next_generation = next;
                            self.peer = PeerLifecycle::Pending(next);
                            self.signaling = Some(ModelSignaling::Answering(false));
                        }
                    }
                }
            }
            ProductAction::BeginAdmission(cb) => {
                if let (Some(gen), PeerLifecycle::Pending(cur)) = (self.callback_gen(cb), self.peer)
                {
                    if gen == cur {
                        self.peer = PeerLifecycle::Admitting(cur);
                        self.signaling = None;
                    }
                }
            }
            ProductAction::CommitAdmission(cb) => {
                if let (Some(gen), PeerLifecycle::Admitting(cur)) =
                    (self.callback_gen(cb), self.peer)
                {
                    if gen == cur {
                        self.peer = PeerLifecycle::Active(cur);
                    }
                }
            }
            ProductAction::Terminal(cb) => {
                if let Some(gen) = self.callback_gen(cb) {
                    if matches!(
                        self.peer,
                        PeerLifecycle::Pending(cur)
                            | PeerLifecycle::Admitting(cur)
                            | PeerLifecycle::Active(cur)
                            if cur == gen
                    ) {
                        self.previous_generation = Some(gen);
                        self.peer = PeerLifecycle::Absent;
                        self.signaling = None;
                    }
                }
            }
            ProductAction::Timeout => match self.peer {
                PeerLifecycle::Pending(cur) | PeerLifecycle::Admitting(cur) => {
                    self.previous_generation = Some(cur);
                    self.peer = PeerLifecycle::Absent;
                    self.signaling = None;
                }
                PeerLifecycle::Absent | PeerLifecycle::Active(_) => {}
            },
        }
        self
    }

    fn callback_gen(self, cb: CallbackGeneration) -> Option<u64> {
        match cb {
            CallbackGeneration::Current => match self.peer {
                PeerLifecycle::Absent => None,
                PeerLifecycle::Pending(g)
                | PeerLifecycle::Admitting(g)
                | PeerLifecycle::Active(g) => Some(g),
            },
            CallbackGeneration::Previous => self.previous_generation,
        }
    }

    fn view(self) -> ProductView {
        let superseded_by_peer = self
            .previous_generation
            .is_some_and(|prev| match self.peer {
                PeerLifecycle::Pending(cur) => {
                    cur != prev && matches!(self.signaling, Some(ModelSignaling::Answering(_)))
                }
                PeerLifecycle::Admitting(cur) | PeerLifecycle::Active(cur) => cur != prev,
                PeerLifecycle::Absent => false,
            });
        ProductView {
            peer: self.peer,
            signaling: self.signaling,
            superseded_by_peer,
        }
    }
}

#[derive(Clone)]
struct ProductImpl {
    smaller_did: Did,
    larger_did: Did,
    peer: Did,
    tx_id: uuid::Uuid,
    lifecycles: ConnectionLifecycleRegistry,
    previous_attempt: Option<PendingConnectionAttempt>,
    now_ms: i64,
}

impl ProductImpl {
    fn new() -> Self {
        let smaller_did = Did::from(1_u32);
        let larger_did = Did::from(2_u32);
        Self {
            smaller_did,
            larger_did,
            peer: larger_did,
            tx_id: uuid::Uuid::new_v4(),
            lifecycles: ConnectionLifecycleRegistry::new(LifecycleBounds::new(1, 1)),
            previous_attempt: None,
            now_ms: 0,
        }
    }

    fn callback_attempt(&self, cb: CallbackGeneration) -> Option<PendingConnectionAttempt> {
        match cb {
            CallbackGeneration::Current => self.lifecycles.state(self.peer).map(|s| s.attempt()),
            CallbackGeneration::Previous => self.previous_attempt,
        }
    }

    fn apply(mut self, action: ProductAction) -> Self {
        match action {
            ProductAction::ReserveOffering(owner) => {
                if self
                    .lifecycles
                    .reserve_for(self.peer, self.now_ms, Handshake::offering(owner))
                    .is_ok()
                {
                    self.now_ms += 1;
                }
            }
            ProductAction::ReserveAnswering => {
                if self
                    .lifecycles
                    .reserve_for(self.peer, self.now_ms, Handshake::answering())
                    .is_ok()
                {
                    self.now_ms += 1;
                }
            }
            ProductAction::MarkSent(cb) => {
                if let Some(attempt) = self.callback_attempt(cb) {
                    let _ = self.lifecycles.mark_offer_sent(
                        attempt,
                        0,
                        bytes::Bytes::from_static(b"hs0"),
                        self.tx_id,
                        None,
                    );
                }
            }
            ProductAction::MarkContinuationSent(cb) => {
                if let Some(attempt) = self.callback_attempt(cb) {
                    let _ = self.lifecycles.mark_offer_sent(
                        attempt,
                        2,
                        bytes::Bytes::from_static(b"hs2"),
                        self.tx_id,
                        Some([9u8; 16]),
                    );
                }
            }
            ProductAction::MarkAnswered(cb) => {
                if let Some(attempt) = self.callback_attempt(cb) {
                    let _ = self.lifecycles.mark_offer_answered(attempt, None);
                }
            }
            ProductAction::AwaitAnsweringContinuation(cb) => {
                if let Some(attempt) = self.callback_attempt(cb) {
                    let _ = self
                        .lifecycles
                        .set_answering_awaiting_step(attempt, 2, [9u8; 16]);
                }
            }
            ProductAction::ClaimAnsweringContinuation => {
                let _ = self
                    .lifecycles
                    .claim_answering_continuation(self.peer, 2, [9u8; 16]);
            }
            ProductAction::CrossingOffer {
                carrier,
                local_smaller,
            } => {
                if let Some(attempt) = self.lifecycles.pending_attempt(self.peer) {
                    if let Some(handshake) = self.lifecycles.handshake(attempt).cloned() {
                        let (local, remote) = if local_smaller {
                            (self.smaller_did, self.larger_did)
                        } else {
                            (self.larger_did, self.smaller_did)
                        };
                        if crossing_offer_verdict(&handshake, carrier, local, remote)
                            == CrossingOfferVerdict::Yield
                        {
                            if let Ok(Some(_answering)) =
                                self.lifecycles
                                    .replace_pending_in(attempt, &handshake, self.now_ms)
                            {
                                self.previous_attempt = Some(attempt);
                                self.now_ms += 1;
                            }
                        }
                    }
                }
            }
            ProductAction::BeginAdmission(cb) => {
                if let Some(attempt) = self.callback_attempt(cb) {
                    let _ = self.lifecycles.begin_admission(attempt);
                }
            }
            ProductAction::CommitAdmission(cb) => {
                if let Some(attempt) = self.callback_attempt(cb) {
                    if let Some(admitting) = self.lifecycles.admitting_connection(attempt) {
                        admitting.activate();
                    }
                }
            }
            ProductAction::Terminal(cb) => {
                if let Some(attempt) = self.callback_attempt(cb) {
                    let removed_unadmitted = self.lifecycles.remove_unadmitted(attempt);
                    let removed_active = self.lifecycles.remove_active(attempt);
                    if removed_unadmitted || removed_active {
                        self.previous_attempt = Some(attempt);
                    }
                }
            }
            ProductAction::Timeout => {
                let expired = self
                    .lifecycles
                    .expire(self.now_ms + PENDING_CONNECTION_TIMEOUT_MS);
                if let Some(first) = expired.into_iter().next() {
                    self.previous_attempt = Some(first.attempt);
                }
            }
        }
        self
    }

    fn view(&self) -> ProductView {
        let peer = match self.lifecycles.state(self.peer) {
            Some(PeerSlotLifecycle::Pending { attempt, .. }) => {
                PeerLifecycle::Pending(attempt.generation)
            }
            Some(PeerSlotLifecycle::Admitting { attempt, .. }) => {
                PeerLifecycle::Admitting(attempt.generation)
            }
            Some(PeerSlotLifecycle::Active { attempt }) => {
                PeerLifecycle::Active(attempt.generation)
            }
            None => PeerLifecycle::Absent,
        };
        let signaling = self
            .lifecycles
            .state(self.peer)
            .and_then(|s| self.lifecycles.handshake(s.attempt()))
            .map(|h| match h {
                Handshake::Offering {
                    owner,
                    phase: handshake::OfferPhase::Creating,
                } => ModelSignaling::Creating(*owner),
                Handshake::Offering {
                    owner,
                    phase: handshake::OfferPhase::Sent { step_index: 0, .. },
                } => ModelSignaling::Sent(*owner),
                Handshake::Offering {
                    owner,
                    phase: handshake::OfferPhase::Sent { .. },
                } => ModelSignaling::SentContinuation(*owner),
                Handshake::Offering {
                    owner,
                    phase: handshake::OfferPhase::Answered,
                } => ModelSignaling::Answered(*owner),
                Handshake::Answering { awaiting_step } => {
                    ModelSignaling::Answering(awaiting_step.is_some())
                }
            });
        let superseded_by_peer = self
            .previous_attempt
            .is_some_and(|prev| self.lifecycles.superseded_by_peer(prev));
        ProductView {
            peer,
            signaling,
            superseded_by_peer,
        }
    }
}

#[test]
fn test_signaling_and_lifecycle_product_model_preserves_invariants() {
    const MAX_DEPTH: usize = 4;
    let actions = [
        ProductAction::ReserveOffering(OfferOwner::Overlay),
        ProductAction::ReserveOffering(OfferOwner::External),
        ProductAction::ReserveAnswering,
        ProductAction::MarkSent(CallbackGeneration::Current),
        ProductAction::MarkContinuationSent(CallbackGeneration::Current),
        ProductAction::MarkAnswered(CallbackGeneration::Current),
        ProductAction::AwaitAnsweringContinuation(CallbackGeneration::Current),
        ProductAction::ClaimAnsweringContinuation,
        ProductAction::CrossingOffer {
            carrier: OfferOwner::Overlay,
            local_smaller: true,
        },
        ProductAction::CrossingOffer {
            carrier: OfferOwner::Overlay,
            local_smaller: false,
        },
        ProductAction::CrossingOffer {
            carrier: OfferOwner::External,
            local_smaller: true,
        },
        ProductAction::CrossingOffer {
            carrier: OfferOwner::External,
            local_smaller: false,
        },
        ProductAction::BeginAdmission(CallbackGeneration::Current),
        ProductAction::BeginAdmission(CallbackGeneration::Previous),
        ProductAction::CommitAdmission(CallbackGeneration::Current),
        ProductAction::Terminal(CallbackGeneration::Current),
        ProductAction::Terminal(CallbackGeneration::Previous),
        ProductAction::Timeout,
    ];

    fn explore_product(
        model: ProductModel,
        implementation: ProductImpl,
        actions: &[ProductAction],
        remaining_depth: usize,
    ) {
        let model_view = model.view();
        let impl_view = implementation.view();
        assert_eq!(model_view, impl_view);
        // Invariant (Signaling): signaling state exists iff the peer is in `Pending`.
        assert_eq!(
            model_view.signaling.is_some(),
            matches!(model_view.peer, PeerLifecycle::Pending(_))
        );
        if remaining_depth == 0 {
            return;
        }
        for &action in actions {
            explore_product(
                model.apply(action),
                implementation.clone().apply(action),
                actions,
                remaining_depth - 1,
            );
        }
    }

    explore_product(
        ProductModel::default(),
        ProductImpl::new(),
        &actions,
        MAX_DEPTH,
    );
}
