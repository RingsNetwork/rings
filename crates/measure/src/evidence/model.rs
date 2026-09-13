//! Executable model and production refinement for the evidence-store relation.
//!
//! The finite candidate set contains an exact duplicate, a freshness conflict,
//! pair-local and global pressure, an oversized record, and an empty record.
//! Stateright explores every candidate ordering and every possible placement of
//! one hard-crash restart. Each successful admission is already durable, so
//! every abstract history is replayed through the real
//! [`ProvisionalEvidenceStore`], snapshot restore, and pagination API.
//! `Admit` denotes a completed external operation: the runtime storage `put` is
//! its linearization point, and a crash during an incomplete operation may
//! refine either the before-state or the conservatively persisted after-state.

use stateright::Checker;
use stateright::Model;
use stateright::Property;

use super::*;

const INITIAL_CREDIT: u8 = 7;
const INITIAL_ROUTE: u8 = 11;
const CANDIDATE_COUNT: u8 = 7;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ModelRecord {
    pair: u8,
    freshness: u8,
    digest: u8,
    observed_at: u8,
    bytes: u8,
    replay_floor: u8,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct ModelMarker {
    pair: u8,
    freshness: u8,
    digest: u8,
}

impl From<ModelRecord> for ModelMarker {
    fn from(record: ModelRecord) -> Self {
        Self {
            pair: record.pair,
            freshness: record.freshness,
            digest: record.digest,
        }
    }
}

const fn candidate(id: u8) -> ModelRecord {
    match id {
        // Candidates 0 and 1 are the same canonical receipt.
        0 | 1 => ModelRecord {
            pair: 0,
            freshness: 1,
            digest: 0,
            observed_at: 1,
            bytes: 2,
            replay_floor: 0,
        },
        // Same freshness as candidate 0, but a different digest and provider.
        2 => ModelRecord {
            pair: 1,
            freshness: 1,
            digest: 1,
            observed_at: 1,
            bytes: 2,
            replay_floor: 0,
        },
        // A second record for pair 0 exercises the per-pair bound.
        3 => ModelRecord {
            pair: 0,
            freshness: 2,
            digest: 2,
            observed_at: 2,
            bytes: 3,
            replay_floor: 1,
        },
        // Distinct pairs exercise the global count and byte bounds.
        4 => ModelRecord {
            pair: 2,
            freshness: 3,
            digest: 3,
            observed_at: 3,
            bytes: 3,
            replay_floor: 2,
        },
        5 => ModelRecord {
            pair: 3,
            freshness: 4,
            digest: 4,
            observed_at: 4,
            bytes: 4,
            replay_floor: 3,
        },
        // Empty canonical bytes are structurally invalid.
        _ => ModelRecord {
            pair: 4,
            freshness: 5,
            digest: 5,
            observed_at: 5,
            bytes: 0,
            replay_floor: 4,
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Action {
    Admit(u8),
    CrashRestart,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Outcome {
    Admitted,
    Duplicate,
    Conflict(u8),
    Empty,
    TooLarge,
    Stale,
    ReplayCapacityExhausted,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ModelCounters {
    admitted: u8,
    duplicates: u8,
    conflicts: u8,
    evicted_records: u8,
    evicted_bytes: u8,
    rejected_records: u8,
    replay_capacity_rejections: u8,
    rejected_replay_markers: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoreState {
    records: Vec<ModelRecord>,
    markers: Vec<ModelMarker>,
    replay_floor: u8,
    outcomes: Vec<Outcome>,
    counters: ModelCounters,
    credit: u8,
    route: u8,
}

impl StoreState {
    fn initial() -> Self {
        Self {
            records: Vec::new(),
            markers: Vec::new(),
            replay_floor: 0,
            outcomes: Vec::new(),
            counters: ModelCounters::default(),
            credit: INITIAL_CREDIT,
            route: INITIAL_ROUTE,
        }
    }

    fn admit(&mut self, record: ModelRecord) {
        if record.bytes == 0 {
            self.counters.rejected_records += 1;
            self.outcomes.push(Outcome::Empty);
            return;
        }
        if usize::from(record.bytes) > effective_record_limit() {
            self.counters.rejected_records += 1;
            self.outcomes.push(Outcome::TooLarge);
            return;
        }
        self.advance_replay_floor(record.replay_floor);
        if record.freshness < self.replay_floor {
            self.counters.rejected_records += 1;
            self.outcomes.push(Outcome::Stale);
            return;
        }
        if self
            .markers
            .iter()
            .any(|marker| marker.digest == record.digest)
        {
            self.counters.duplicates += 1;
            self.outcomes.push(Outcome::Duplicate);
            return;
        }
        if let Some(retained) = self
            .markers
            .iter()
            .find(|marker| marker.freshness == record.freshness)
            .map(|marker| marker.digest)
        {
            self.counters.conflicts += 1;
            self.outcomes.push(Outcome::Conflict(retained));
            return;
        }
        if self.replay_over_bound_after() {
            self.counters.replay_capacity_rejections += 1;
            self.outcomes.push(Outcome::ReplayCapacityExhausted);
            return;
        }

        self.markers.push(record.into());
        self.markers.sort();
        self.records.push(record);
        self.records.sort_by_key(|resident| resident.digest);
        self.counters.admitted += 1;
        self.evict_while(
            |state| state.pair_over_bound(record.pair),
            record.digest,
            Some(record.pair),
        );
        self.evict_while(Self::global_over_bound, record.digest, None);
        self.outcomes.push(Outcome::Admitted);
    }

    fn advance_replay_floor(&mut self, floor: u8) {
        self.replay_floor = self.replay_floor.max(floor);
        self.markers.retain(|marker| {
            marker.freshness >= self.replay_floor
                || self
                    .records
                    .iter()
                    .any(|record| record.digest == marker.digest)
        });
    }

    fn replay_over_bound_after(&self) -> bool {
        let next_global = self.markers.len().saturating_add(1);
        let next_beneficiary = next_global;
        next_global > model_limits().max_replay_markers.get()
            || next_beneficiary > model_limits().max_replay_markers_per_beneficiary.get()
    }

    fn evict_while(
        &mut self,
        over_bound: impl Fn(&Self) -> bool,
        candidate_digest: u8,
        pair: Option<u8>,
    ) {
        while over_bound(self) {
            let victim = self
                .records
                .iter()
                .filter(|record| {
                    record.digest != candidate_digest && pair.is_none_or(|pair| record.pair == pair)
                })
                .min_by_key(|record| (record.observed_at, record.digest))
                .copied();
            let Some(victim) = victim else {
                break;
            };
            self.records.retain(|record| record.digest != victim.digest);
            self.counters.evicted_records += 1;
            self.counters.evicted_bytes += victim.bytes;
        }
    }

    fn pair_over_bound(&self, pair: u8) -> bool {
        let records = self
            .records
            .iter()
            .filter(|record| record.pair == pair)
            .collect::<Vec<_>>();
        records.len() > model_limits().max_records_per_pair.get()
            || records
                .iter()
                .map(|record| usize::from(record.bytes))
                .sum::<usize>()
                > model_limits().max_bytes_per_pair.get()
    }

    fn global_over_bound(&self) -> bool {
        self.records.len() > model_limits().max_records.get()
            || self.resident_bytes() > model_limits().max_bytes.get()
    }

    fn resident_bytes(&self) -> usize {
        self.records
            .iter()
            .map(|record| usize::from(record.bytes))
            .sum()
    }

    fn freshness_is_unique(&self) -> bool {
        self.markers.iter().enumerate().all(|(index, marker)| {
            self.markers
                .iter()
                .skip(index.saturating_add(1))
                .all(|other| other.freshness != marker.freshness)
        })
    }
}

#[derive(Clone)]
struct EvidenceModel;

impl Model for EvidenceModel {
    type State = Vec<Action>;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![Vec::new()]
    }

    fn actions(&self, history: &Self::State, actions: &mut Vec<Self::Action>) {
        for id in 0..CANDIDATE_COUNT {
            if !history.contains(&Action::Admit(id)) {
                actions.push(Action::Admit(id));
            }
        }
        if !history.contains(&Action::CrashRestart) {
            actions.push(Action::CrashRestart);
        }
    }

    fn next_state(&self, history: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut next = history.clone();
        next.push(action);
        Some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always(
                "production refines every abstract transition",
                |_, history| production_refines(history),
            ),
            Property::<Self>::always("global and per-pair hard bounds hold", |_, history| {
                let state = abstract_replay(history);
                !state.global_over_bound() && (0..=4).all(|pair| !state.pair_over_bound(pair))
            }),
            Property::<Self>::always("durable replay-marker bounds hold", |_, history| {
                let state = abstract_replay(history);
                state.markers.len() <= model_limits().max_replay_markers.get()
                    && state.markers.len()
                        <= model_limits().max_replay_markers_per_beneficiary.get()
            }),
            Property::<Self>::always("one digest owns each freshness key", |_, history| {
                abstract_replay(history).freshness_is_unique()
            }),
            Property::<Self>::always(
                "an admitted digest is never admitted twice",
                |_, history| no_digest_is_admitted_twice(history),
            ),
            Property::<Self>::always(
                "every still-admissible accepted key keeps its marker",
                |_, history| active_admissions_remain_marked(history),
            ),
            Property::<Self>::always("the replay floor is monotonic", |_, history| {
                replay_floor_is_monotonic(history)
            }),
            Property::<Self>::always(
                "an admitted candidate survives its transition",
                |_, history| {
                    let Some(Action::Admit(id)) = history.last() else {
                        return true;
                    };
                    let state = abstract_replay(history);
                    state.outcomes.last() != Some(&Outcome::Admitted)
                        || state
                            .records
                            .iter()
                            .any(|record| record.digest == candidate(*id).digest)
                },
            ),
            Property::<Self>::always("evidence cannot affect credit or routing", |_, history| {
                let state = abstract_replay(history);
                state.credit == INITIAL_CREDIT && state.route == INITIAL_ROUTE
            }),
            Property::<Self>::sometimes("duplicate is reachable", |_, history| {
                abstract_replay(history)
                    .outcomes
                    .contains(&Outcome::Duplicate)
            }),
            Property::<Self>::sometimes("freshness conflict is reachable", |_, history| {
                abstract_replay(history)
                    .outcomes
                    .iter()
                    .any(|outcome| matches!(outcome, Outcome::Conflict(_)))
            }),
            Property::<Self>::sometimes("bounded eviction is reachable", |_, history| {
                abstract_replay(history).counters.evicted_records > 0
            }),
            Property::<Self>::sometimes(
                "an evicted receipt remains duplicate-protected",
                |_, history| evicted_duplicate_is_rejected(history),
            ),
            Property::<Self>::sometimes("replay capacity fails closed", |_, history| {
                abstract_replay(history)
                    .outcomes
                    .contains(&Outcome::ReplayCapacityExhausted)
            }),
            Property::<Self>::sometimes("clock regression fails closed", |_, history| {
                abstract_replay(history).outcomes.contains(&Outcome::Stale)
            }),
            Property::<Self>::sometimes(
                "an expired evicted marker is eventually pruned",
                |_, history| expired_evicted_marker_is_pruned(history),
            ),
            Property::<Self>::sometimes("invalid records are rejected", |_, history| {
                abstract_replay(history).counters.rejected_records == 2
            }),
        ]
    }
}

fn abstract_replay(history: &[Action]) -> StoreState {
    let mut state = StoreState::initial();
    for action in history {
        if let Action::Admit(id) = action {
            state.admit(candidate(*id));
        }
    }
    state
}

fn no_digest_is_admitted_twice(history: &[Action]) -> bool {
    let mut state = StoreState::initial();
    let mut admitted = Vec::new();
    for action in history {
        let Action::Admit(id) = action else {
            continue;
        };
        state.admit(candidate(*id));
        if state.outcomes.last() == Some(&Outcome::Admitted) {
            let digest = candidate(*id).digest;
            if admitted.contains(&digest) {
                return false;
            }
            admitted.push(digest);
        }
    }
    true
}

fn active_admissions_remain_marked(history: &[Action]) -> bool {
    let mut state = StoreState::initial();
    let mut admitted = Vec::new();
    for action in history {
        let Action::Admit(id) = action else {
            continue;
        };
        let record = candidate(*id);
        state.admit(record);
        if state.outcomes.last() == Some(&Outcome::Admitted) {
            admitted.push(ModelMarker::from(record));
        }
    }
    admitted
        .into_iter()
        .all(|marker| marker.freshness < state.replay_floor || state.markers.contains(&marker))
}

fn replay_floor_is_monotonic(history: &[Action]) -> bool {
    let mut state = StoreState::initial();
    let mut prior = state.replay_floor;
    for action in history {
        if let Action::Admit(id) = action {
            state.admit(candidate(*id));
            if state.replay_floor < prior {
                return false;
            }
            prior = state.replay_floor;
        }
    }
    true
}

fn expired_evicted_marker_is_pruned(history: &[Action]) -> bool {
    let state = abstract_replay(history);
    let mut prefix = StoreState::initial();
    let mut admitted_first = false;
    for action in history {
        let Action::Admit(id) = action else {
            continue;
        };
        prefix.admit(candidate(*id));
        if *id == 0 {
            admitted_first = prefix.outcomes.last() == Some(&Outcome::Admitted);
            break;
        }
    }
    admitted_first
        && state.replay_floor > candidate(0).freshness
        && !state
            .records
            .iter()
            .any(|record| record.digest == candidate(0).digest)
        && !state
            .markers
            .iter()
            .any(|marker| marker.digest == candidate(0).digest)
}

fn evicted_duplicate_is_rejected(history: &[Action]) -> bool {
    let mut state = StoreState::initial();
    let mut first_was_evicted = false;
    for action in history {
        let Action::Admit(id) = action else {
            continue;
        };
        if *id == 1 && first_was_evicted {
            state.admit(candidate(*id));
            return state.outcomes.last() == Some(&Outcome::Duplicate);
        }
        state.admit(candidate(*id));
        first_was_evicted = state
            .markers
            .iter()
            .any(|marker| marker.digest == candidate(0).digest)
            && !state
                .records
                .iter()
                .any(|record| record.digest == candidate(0).digest);
    }
    false
}

fn model_limits() -> EvidenceLimits {
    EvidenceLimits {
        max_records: nonzero(2),
        max_bytes: nonzero(5),
        max_records_per_pair: nonzero(1),
        max_bytes_per_pair: nonzero(3),
        max_replay_markers: nonzero(2),
        max_replay_markers_per_beneficiary: nonzero(2),
    }
}

fn effective_record_limit() -> usize {
    model_limits()
        .max_bytes
        .get()
        .min(model_limits().max_bytes_per_pair.get())
}

fn production_record(record: ModelRecord) -> ProvisionalEvidenceRecord<u8> {
    ProvisionalEvidenceRecord::new(
        EvidenceAccountPair::new(record.pair, 9),
        EvidenceFreshnessKey::new(1, 9, u64::from(record.freshness), [record.freshness; 32]),
        EvidenceDigest::new([record.digest.saturating_add(1); 32]),
        vec![record.digest; usize::from(record.bytes)],
        UnixTime::from_secs(u64::from(record.observed_at)),
        u64::from(record.replay_floor),
    )
}

fn outcome_from_report(report: EvidenceAdmissionReport<u8>) -> Outcome {
    match report.admission() {
        EvidenceAdmission::Admitted => Outcome::Admitted,
        EvidenceAdmission::Duplicate => Outcome::Duplicate,
        EvidenceAdmission::Conflict { retained } => {
            Outcome::Conflict(retained.into_bytes()[0].saturating_sub(1))
        }
        EvidenceAdmission::ReplayCapacityExhausted => Outcome::ReplayCapacityExhausted,
    }
}

fn projection(store: &ProvisionalEvidenceStore<u8>, outcomes: Vec<Outcome>) -> StoreState {
    let records = store
        .page(None, nonzero(CANDIDATE_COUNT as usize))
        .records()
        .iter()
        .map(|record| ModelRecord {
            pair: *record.pair().provider(),
            freshness: u8::try_from(record.freshness().epoch_slot()).unwrap_or(u8::MAX),
            digest: record.digest().into_bytes()[0].saturating_sub(1),
            observed_at: u8::try_from(record.observed_at().as_secs()).unwrap_or(u8::MAX),
            bytes: u8::try_from(record.canonical_receipt().len()).unwrap_or(u8::MAX),
            replay_floor: u8::try_from(record.replay_floor()).unwrap_or(u8::MAX),
        })
        .collect();
    let snapshot = store.snapshot();
    let mut markers = snapshot
        .replay_markers
        .iter()
        .map(|marker| ModelMarker {
            pair: *marker.pair().provider(),
            freshness: u8::try_from(marker.freshness().epoch_slot()).unwrap_or(u8::MAX),
            digest: marker.digest().into_bytes()[0].saturating_sub(1),
        })
        .collect::<Vec<_>>();
    markers.sort();
    let counters = store.counters();
    StoreState {
        records,
        markers,
        replay_floor: u8::try_from(store.replay_floor()).unwrap_or(u8::MAX),
        outcomes,
        counters: ModelCounters {
            admitted: u8::try_from(counters.admitted()).unwrap_or(u8::MAX),
            duplicates: u8::try_from(counters.duplicates()).unwrap_or(u8::MAX),
            conflicts: u8::try_from(counters.conflicts()).unwrap_or(u8::MAX),
            evicted_records: u8::try_from(counters.evicted_records()).unwrap_or(u8::MAX),
            evicted_bytes: u8::try_from(counters.evicted_bytes()).unwrap_or(u8::MAX),
            rejected_records: u8::try_from(counters.rejected_records()).unwrap_or(u8::MAX),
            replay_capacity_rejections: u8::try_from(counters.replay_capacity_rejections())
                .unwrap_or(u8::MAX),
            rejected_replay_markers: u8::try_from(counters.rejected_replay_markers())
                .unwrap_or(u8::MAX),
        },
        credit: INITIAL_CREDIT,
        route: INITIAL_ROUTE,
    }
}

fn production_refines(history: &[Action]) -> bool {
    let mut store = ProvisionalEvidenceStore::new(model_limits());
    let mut outcomes = Vec::new();
    for action in history {
        match action {
            Action::Admit(id) => match store.admit(production_record(candidate(*id))) {
                Ok(report) => outcomes.push(outcome_from_report(report)),
                Err(EvidenceError::EmptyReceipt) => outcomes.push(Outcome::Empty),
                Err(EvidenceError::ReceiptTooLarge { .. }) => outcomes.push(Outcome::TooLarge),
                Err(EvidenceError::FreshnessBeforeReplayFloor { .. }) => {
                    outcomes.push(Outcome::Stale)
                }
                Err(EvidenceError::StorageUnavailable)
                | Err(EvidenceError::UnsupportedSnapshotVersion { .. })
                | Err(EvidenceError::ReplayMarkerLimitExceeded { .. })
                | Err(EvidenceError::PersistenceUnavailable) => return false,
            },
            Action::CrashRestart => {
                let Ok((restored, report)) = ProvisionalEvidenceStore::from_snapshot_with_validator(
                    store.snapshot(),
                    model_limits(),
                    |_| true,
                    |_| true,
                ) else {
                    return false;
                };
                if report != EvidenceLoadReport::default() {
                    return false;
                }
                store = restored;
            }
        }
    }
    projection(&store, outcomes) == abstract_replay(history) && pagination_matches(&store)
}

fn pagination_matches(store: &ProvisionalEvidenceStore<u8>) -> bool {
    let expected = store
        .page(None, nonzero(CANDIDATE_COUNT as usize))
        .records()
        .iter()
        .map(ProvisionalEvidenceRecord::digest)
        .collect::<Vec<_>>();
    let mut actual = Vec::new();
    let mut cursor = None;
    loop {
        let page = store.page(cursor, nonzero(1));
        actual.extend(page.records().iter().map(ProvisionalEvidenceRecord::digest));
        let Some(next) = page.next_cursor() else {
            break;
        };
        cursor = Some(next);
    }
    actual == expected
}

#[test]
fn model_checks_every_bounded_admission_and_restart_schedule() {
    EvidenceModel
        .checker()
        .spawn_bfs()
        .join()
        .assert_properties();
}
