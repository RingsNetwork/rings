//! Executable model and production refinement for the evidence-store relation.
//!
//! The finite candidate set contains an exact duplicate, a freshness conflict,
//! pair-local and global pressure, distinct beneficiaries, replay-capacity and
//! clock-regression schedules, and an empty record. Structural size rejection
//! is covered by the pure-store tests without multiplying the state space here.
//! Stateright explores every candidate ordering and every possible placement of
//! one hard-crash restart. Each successful admission is already durable, so
//! every abstract history is replayed through the real
//! [`ProvisionalEvidenceStore`], snapshot restore, and pagination API.
//! `Admit` denotes a completed external operation: the runtime storage `put` is
//! its linearization point, and a crash during an incomplete operation may
//! refine either the before-state or the conservatively persisted after-state.

use std::collections::BTreeSet;

use stateright::Checker;
use stateright::Expectation;
use stateright::Model;
use stateright::Property;

use super::*;

const CANDIDATE_COUNT: u8 = 7;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Mutant {
    CounterDrift,
    SkipEviction,
    IgnoreReplayCapacity,
    AllowFreshnessConflict,
    PruneResidentMarker,
    AllowDuplicateDigest,
    DropEvictedMarkersOnRestart,
    OverwriteReplayFloor,
    EvictAdmittedCandidate,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ModelRecord {
    provider: u8,
    beneficiary: u8,
    freshness: u8,
    digest: u8,
    observed_at: u8,
    bytes: u8,
    replay_floor: u8,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct ModelMarker {
    provider: u8,
    beneficiary: u8,
    freshness: u8,
    digest: u8,
}

impl From<ModelRecord> for ModelMarker {
    fn from(record: ModelRecord) -> Self {
        Self {
            provider: record.provider,
            beneficiary: record.beneficiary,
            freshness: record.freshness,
            digest: record.digest,
        }
    }
}

const fn candidate(id: u8) -> ModelRecord {
    match id {
        // Candidates 0 and 1 are the same canonical receipt.
        0 | 1 => ModelRecord {
            provider: 0,
            beneficiary: 0,
            freshness: 1,
            digest: 0,
            observed_at: 1,
            bytes: 2,
            replay_floor: 0,
        },
        // Same freshness as candidate 0, but a different digest and provider.
        2 => ModelRecord {
            provider: 1,
            beneficiary: 0,
            freshness: 1,
            digest: 1,
            observed_at: 1,
            bytes: 2,
            replay_floor: 0,
        },
        // A second record for pair 0 exercises the per-pair bound.
        3 => ModelRecord {
            provider: 0,
            beneficiary: 0,
            freshness: 2,
            digest: 2,
            observed_at: 2,
            bytes: 3,
            replay_floor: 1,
        },
        // Distinct pairs exercise the global count and byte bounds.
        4 => ModelRecord {
            provider: 2,
            beneficiary: 1,
            freshness: 3,
            digest: 3,
            observed_at: 3,
            bytes: 3,
            replay_floor: 2,
        },
        // A third provider for beneficiary 0 separates its marker bound from
        // the global marker bound exercised with the other beneficiaries.
        5 => ModelRecord {
            provider: 3,
            beneficiary: 0,
            freshness: 4,
            digest: 4,
            observed_at: 4,
            bytes: 3,
            replay_floor: 1,
        },
        // Empty canonical bytes are structurally invalid.
        _ => ModelRecord {
            provider: 4,
            beneficiary: 3,
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
}

impl StoreState {
    fn initial() -> Self {
        Self {
            records: Vec::new(),
            markers: Vec::new(),
            replay_floor: 0,
            outcomes: Vec::new(),
            counters: ModelCounters::default(),
        }
    }

    fn admit(&mut self, record: ModelRecord, mutant: Option<Mutant>) {
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
        self.advance_replay_floor(record.replay_floor, mutant);
        if record.freshness < self.replay_floor {
            self.counters.rejected_records += 1;
            self.outcomes.push(Outcome::Stale);
            return;
        }
        if mutant != Some(Mutant::AllowDuplicateDigest)
            && self
                .markers
                .iter()
                .any(|marker| marker.digest == record.digest)
        {
            self.counters.duplicates += 1;
            self.outcomes.push(Outcome::Duplicate);
            return;
        }
        if mutant != Some(Mutant::AllowFreshnessConflict) {
            if let Some(retained) = self
                .markers
                .iter()
                .find(|marker| {
                    marker.freshness == record.freshness && marker.digest != record.digest
                })
                .map(|marker| marker.digest)
            {
                self.counters.conflicts += 1;
                self.outcomes.push(Outcome::Conflict(retained));
                return;
            }
        }
        if mutant != Some(Mutant::IgnoreReplayCapacity)
            && self.replay_over_bound_after(record.beneficiary)
        {
            self.counters.replay_capacity_rejections += 1;
            self.outcomes.push(Outcome::ReplayCapacityExhausted);
            return;
        }

        if !self
            .markers
            .iter()
            .any(|marker| marker.digest == record.digest)
        {
            self.markers.push(record.into());
            self.markers.sort();
        }
        if !self
            .records
            .iter()
            .any(|resident| resident.digest == record.digest)
        {
            self.records.push(record);
            self.records.sort_by_key(|resident| resident.digest);
        }
        self.counters.admitted += if mutant == Some(Mutant::CounterDrift) {
            2
        } else {
            1
        };
        if mutant != Some(Mutant::SkipEviction) {
            let protected =
                (mutant != Some(Mutant::EvictAdmittedCandidate)).then_some(record.digest);
            self.evict_while(
                |state| state.pair_over_bound(record.provider, record.beneficiary),
                protected,
                Some((record.provider, record.beneficiary)),
            );
            self.evict_while(Self::global_over_bound, protected, None);
        }
        self.outcomes.push(Outcome::Admitted);
    }

    fn advance_replay_floor(&mut self, floor: u8, mutant: Option<Mutant>) {
        if mutant == Some(Mutant::OverwriteReplayFloor) {
            self.replay_floor = floor;
        } else {
            self.replay_floor = self.replay_floor.max(floor);
        }
        self.prune_expired_evicted_markers(mutant);
    }

    fn reconcile_restart(&mut self, mutant: Option<Mutant>) {
        if mutant == Some(Mutant::DropEvictedMarkersOnRestart) {
            self.markers.retain(|marker| {
                self.records
                    .iter()
                    .any(|record| record.digest == marker.digest)
            });
            return;
        }
        self.prune_expired_evicted_markers(mutant);
    }

    fn prune_expired_evicted_markers(&mut self, mutant: Option<Mutant>) {
        self.markers.retain(|marker| {
            marker.freshness >= self.replay_floor
                || (mutant != Some(Mutant::PruneResidentMarker)
                    && self
                        .records
                        .iter()
                        .any(|record| record.digest == marker.digest))
        });
    }

    fn replay_over_bound_after(&self, beneficiary: u8) -> bool {
        let next_global = self.markers.len().saturating_add(1);
        let next_beneficiary = self
            .markers
            .iter()
            .filter(|marker| marker.beneficiary == beneficiary)
            .count()
            .saturating_add(1);
        next_global > model_limits().max_replay_markers.get()
            || next_beneficiary > model_limits().max_replay_markers_per_beneficiary.get()
    }

    fn evict_while(
        &mut self,
        over_bound: impl Fn(&Self) -> bool,
        protected_digest: Option<u8>,
        pair: Option<(u8, u8)>,
    ) {
        while over_bound(self) {
            let victim = self
                .records
                .iter()
                .filter(|record| {
                    Some(record.digest) != protected_digest
                        && pair.is_none_or(|pair| (record.provider, record.beneficiary) == pair)
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

    fn pair_over_bound(&self, provider: u8, beneficiary: u8) -> bool {
        let records = self
            .records
            .iter()
            .filter(|record| record.provider == provider && record.beneficiary == beneficiary)
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
                .all(|other| {
                    (other.beneficiary, other.freshness) != (marker.beneficiary, marker.freshness)
                })
        })
    }

    fn marker_digests_are_unique(&self) -> bool {
        self.markers.iter().enumerate().all(|(index, marker)| {
            self.markers
                .iter()
                .skip(index.saturating_add(1))
                .all(|other| other.digest != marker.digest)
        })
    }

    fn retained_records_have_markers(&self) -> bool {
        self.records.iter().all(|record| {
            self.markers
                .iter()
                .any(|marker| marker.digest == record.digest)
        })
    }

    fn replay_markers_are_within_bounds(&self) -> bool {
        self.markers.len() <= model_limits().max_replay_markers.get()
            && self.markers.iter().all(|marker| {
                self.markers
                    .iter()
                    .filter(|other| other.beneficiary == marker.beneficiary)
                    .count()
                    <= model_limits().max_replay_markers_per_beneficiary.get()
            })
    }
}

#[derive(Clone, Default)]
struct EvidenceModel {
    mutant: Option<Mutant>,
    candidate_limit: Option<u8>,
}

impl Model for EvidenceModel {
    type State = Vec<Action>;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![Vec::new()]
    }

    fn actions(&self, history: &Self::State, actions: &mut Vec<Self::Action>) {
        let count = self.candidate_limit.unwrap_or(CANDIDATE_COUNT);
        for id in 0..count {
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
                |model, history| {
                    !matches!(model.mutant, None | Some(Mutant::CounterDrift))
                        || production_refines(history, model.mutant)
                },
            ),
            Property::<Self>::always("global and per-pair hard bounds hold", |model, history| {
                let state = abstract_replay(history, model.mutant);
                !state.global_over_bound()
                    && (0..=4).all(|provider| {
                        (0..=3).all(|beneficiary| !state.pair_over_bound(provider, beneficiary))
                    })
            }),
            Property::<Self>::always("durable replay-marker bounds hold", |model, history| {
                abstract_replay(history, model.mutant).replay_markers_are_within_bounds()
            }),
            Property::<Self>::always("one digest owns each freshness key", |model, history| {
                let state = abstract_replay(history, model.mutant);
                state.freshness_is_unique() && state.marker_digests_are_unique()
            }),
            Property::<Self>::always(
                "retained evidence always has a replay marker",
                |model, history| {
                    abstract_replay(history, model.mutant).retained_records_have_markers()
                },
            ),
            Property::<Self>::always(
                "an admitted digest is never admitted twice",
                |model, history| no_digest_is_admitted_twice(history, model.mutant),
            ),
            Property::<Self>::always(
                "every still-admissible accepted key keeps its marker",
                |model, history| active_admissions_remain_marked(history, model.mutant),
            ),
            Property::<Self>::always("the replay floor is monotonic", |model, history| {
                replay_floor_is_monotonic(history, model.mutant)
            }),
            Property::<Self>::always(
                "an admitted candidate survives its transition",
                |model, history| {
                    let Some(Action::Admit(id)) = history.last() else {
                        return true;
                    };
                    let state = abstract_replay(history, model.mutant);
                    state.outcomes.last() != Some(&Outcome::Admitted)
                        || state
                            .records
                            .iter()
                            .any(|record| record.digest == candidate(*id).digest)
                },
            ),
            Property::<Self>::sometimes("duplicate is reachable", |model, history| {
                abstract_replay(history, model.mutant)
                    .outcomes
                    .contains(&Outcome::Duplicate)
            }),
            Property::<Self>::sometimes("freshness conflict is reachable", |model, history| {
                abstract_replay(history, model.mutant)
                    .outcomes
                    .iter()
                    .any(|outcome| matches!(outcome, Outcome::Conflict(_)))
            }),
            Property::<Self>::sometimes("bounded eviction is reachable", |model, history| {
                abstract_replay(history, model.mutant)
                    .counters
                    .evicted_records
                    > 0
            }),
            Property::<Self>::sometimes(
                "an evicted receipt remains duplicate-protected",
                |model, history| evicted_duplicate_is_rejected(history, model.mutant),
            ),
            Property::<Self>::sometimes("replay capacity fails closed", |model, history| {
                abstract_replay(history, model.mutant)
                    .outcomes
                    .contains(&Outcome::ReplayCapacityExhausted)
            }),
            Property::<Self>::sometimes(
                "distinct beneficiaries share the global replay budget",
                |model, history| {
                    let state = abstract_replay(history, model.mutant);
                    state.markers.iter().any(|left| {
                        state
                            .markers
                            .iter()
                            .any(|right| left.beneficiary != right.beneficiary)
                    })
                },
            ),
            Property::<Self>::sometimes("clock regression fails closed", |model, history| {
                abstract_replay(history, model.mutant)
                    .outcomes
                    .contains(&Outcome::Stale)
            }),
            Property::<Self>::sometimes(
                "an expired evicted marker is eventually pruned",
                |model, history| expired_evicted_marker_is_pruned(history, model.mutant),
            ),
            Property::<Self>::sometimes("invalid records are rejected", |model, history| {
                abstract_replay(history, model.mutant)
                    .counters
                    .rejected_records
                    > 0
            }),
        ]
    }
}

fn abstract_replay(history: &[Action], mutant: Option<Mutant>) -> StoreState {
    let mut state = StoreState::initial();
    for action in history {
        match action {
            Action::Admit(id) => state.admit(candidate(*id), mutant),
            Action::CrashRestart => state.reconcile_restart(mutant),
        }
    }
    state
}

fn no_digest_is_admitted_twice(history: &[Action], mutant: Option<Mutant>) -> bool {
    let mut state = StoreState::initial();
    let mut admitted = Vec::new();
    for action in history {
        match action {
            Action::Admit(id) => {
                state.admit(candidate(*id), mutant);
                if state.outcomes.last() == Some(&Outcome::Admitted) {
                    let digest = candidate(*id).digest;
                    if admitted.contains(&digest) {
                        return false;
                    }
                    admitted.push(digest);
                }
            }
            Action::CrashRestart => state.reconcile_restart(mutant),
        }
    }
    true
}

fn active_admissions_remain_marked(history: &[Action], mutant: Option<Mutant>) -> bool {
    let mut state = StoreState::initial();
    let mut admitted = Vec::new();
    for action in history {
        match action {
            Action::Admit(id) => {
                let record = candidate(*id);
                state.admit(record, mutant);
                if state.outcomes.last() == Some(&Outcome::Admitted) {
                    admitted.push(ModelMarker::from(record));
                }
            }
            Action::CrashRestart => state.reconcile_restart(mutant),
        }
    }
    admitted
        .into_iter()
        .all(|marker| marker.freshness < state.replay_floor || state.markers.contains(&marker))
}

fn replay_floor_is_monotonic(history: &[Action], mutant: Option<Mutant>) -> bool {
    let mut state = StoreState::initial();
    let mut prior = state.replay_floor;
    for action in history {
        match action {
            Action::Admit(id) => {
                state.admit(candidate(*id), mutant);
                if state.replay_floor < prior {
                    return false;
                }
                prior = state.replay_floor;
            }
            Action::CrashRestart => state.reconcile_restart(mutant),
        }
    }
    true
}

fn expired_evicted_marker_is_pruned(history: &[Action], mutant: Option<Mutant>) -> bool {
    let state = abstract_replay(history, mutant);
    let mut prefix = StoreState::initial();
    let mut admitted_first = false;
    for action in history {
        match action {
            Action::Admit(id) => {
                prefix.admit(candidate(*id), mutant);
                if *id == 0 {
                    admitted_first = prefix.outcomes.last() == Some(&Outcome::Admitted);
                    break;
                }
            }
            Action::CrashRestart => prefix.reconcile_restart(mutant),
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

fn evicted_duplicate_is_rejected(history: &[Action], mutant: Option<Mutant>) -> bool {
    let mut state = StoreState::initial();
    let mut first_was_evicted = false;
    for action in history {
        match action {
            Action::Admit(id) => {
                if *id == 1 && first_was_evicted {
                    state.admit(candidate(*id), mutant);
                    return matches!(
                        state.outcomes.last(),
                        Some(Outcome::Duplicate | Outcome::Stale)
                    );
                }
                state.admit(candidate(*id), mutant);
            }
            Action::CrashRestart => state.reconcile_restart(mutant),
        }
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
        max_replay_markers: nonzero(3),
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
        EvidenceAccountPair::new(record.provider, record.beneficiary),
        EvidenceFreshnessKey::new(
            1,
            record.beneficiary,
            u64::from(record.freshness),
            [record.freshness; 32],
        ),
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
            provider: *record.pair().provider(),
            beneficiary: *record.pair().beneficiary(),
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
            provider: *marker.pair().provider(),
            beneficiary: *marker.pair().beneficiary(),
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
    }
}

fn production_refines(history: &[Action], mutant: Option<Mutant>) -> bool {
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
    projection(&store, outcomes) == abstract_replay(history, mutant) && pagination_matches(&store)
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

/// The names of the properties `model` refutes or leaves unwitnessed.
fn refuted(model: EvidenceModel) -> BTreeSet<&'static str> {
    let checker = model.checker().spawn_bfs().join();
    checker
        .model()
        .properties()
        .into_iter()
        .filter(|property| {
            let discovered = checker.discovery(property.name).is_some();
            match property.expectation {
                Expectation::Sometimes => !discovered,
                _ => discovered,
            }
        })
        .map(|property| property.name)
        .collect()
}

#[test]
fn model_checks_every_bounded_admission_and_restart_schedule() {
    EvidenceModel::default()
        .checker()
        .spawn_bfs()
        .join()
        .assert_properties();
}

/// Every safety invariant of the evidence-store model is load-bearing: mutating the transition
/// relation is refuted by the property that states the violated law.
#[test]
fn test_each_evidence_store_mutant_is_refuted() {
    for (mutant, candidate_limit, law) in [
        (
            Mutant::CounterDrift,
            Some(2),
            "production refines every abstract transition",
        ),
        (
            Mutant::SkipEviction,
            Some(5),
            "global and per-pair hard bounds hold",
        ),
        (
            Mutant::IgnoreReplayCapacity,
            Some(6),
            "durable replay-marker bounds hold",
        ),
        (
            Mutant::AllowFreshnessConflict,
            Some(3),
            "one digest owns each freshness key",
        ),
        (
            Mutant::PruneResidentMarker,
            Some(5),
            "retained evidence always has a replay marker",
        ),
        (
            Mutant::AllowDuplicateDigest,
            Some(2),
            "an admitted digest is never admitted twice",
        ),
        (
            Mutant::DropEvictedMarkersOnRestart,
            Some(4),
            "every still-admissible accepted key keeps its marker",
        ),
        (
            Mutant::OverwriteReplayFloor,
            Some(6),
            "the replay floor is monotonic",
        ),
        (
            Mutant::EvictAdmittedCandidate,
            Some(4),
            "an admitted candidate survives its transition",
        ),
    ] {
        let model = EvidenceModel {
            mutant: Some(mutant),
            candidate_limit,
        };
        assert!(
            refuted(model).contains(law),
            "{mutant:?} must be refuted by `{law}`"
        );
    }
}
