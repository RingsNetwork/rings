//! The durability law, checked over every crash point of the write and removal plans.
//!
//! A model of one record under crash: each step of a plan takes effect in the page cache at
//! once, and on stable storage only when a later flush covers it. A crash after any prefix of a
//! plan keeps every flushed effect, and keeps or loses each unflushed one independently (the
//! file system promises nothing about the order of unflushed effects):
//!
//! ```text
//! put:    data (WriteTemporary) is durable iff SyncTemporary followed it
//!         rename (Rename) is durable iff SyncDirectory followed it
//!         crash: name holds new  if rename survives ∧ data survives
//!                          torn  if rename survives ∧ ¬data survives
//!                          old   otherwise
//! remove: removal is durable iff SyncDirectory followed it
//! marker: Witness creates and flushes it; Unwitness removes and flushes it
//! ```
//!
//! The presence law (#915) adds one invariant over the same crash points: a durable marker
//! names a record that survives (`P ⊆ R`), so a restore never reads a record that was not
//! made durable here as missing.
//!
//! The law: an authoritative plan leaves the record old or new at every crash point (never
//! torn), and new (or removed) once the plan completes. A disposable plan, which skips the
//! flushes, can tear a record; that case is asserted too, so the model discriminates.
//!
//! A batch (#916) is read per record, each record's effects independent of the others': its
//! data is durable iff a push and then the barrier followed its write (the Apple reading, the
//! weaker one: elsewhere the push alone is durable), and its rename iff a directory flush
//! followed it.

use super::batch::BatchStep;
use super::PutStep;
use super::RecordAuthority;
use super::RemoveStep;

/// What a crash leaves under a record's name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    /// The record before the operation, whole.
    Old,
    /// The record the operation wrote, whole.
    New,
    /// The record's name holds a torn record: the rename survived, its data did not.
    Torn,
    /// The record is gone.
    Removed,
}

/// The effects of a put's prefix: whether its data and its rename were made, and made durable.
#[derive(Clone, Copy, Debug, Default)]
struct PutEffects {
    /// The temporary file was written.
    data: bool,
    /// The temporary file was flushed.
    data_durable: bool,
    /// The temporary file was renamed over the record.
    renamed: bool,
    /// The rename was flushed.
    rename_durable: bool,
    /// The record's presence marker was created and flushed.
    marked: bool,
}

/// The effects of the first `steps` of a put plan (pure).
fn put_effects(steps: &[PutStep]) -> PutEffects {
    steps
        .iter()
        .fold(PutEffects::default(), |mut effects, step| {
            match step {
                PutStep::WriteTemporary => effects.data = true,
                PutStep::SyncTemporary => effects.data_durable = effects.data,
                PutStep::Rename => effects.renamed = true,
                PutStep::SyncDirectory => effects.rename_durable = effects.renamed,
                PutStep::Witness => effects.marked = true,
            }
            effects
        })
}

/// The effects of the first `steps` of a batch plan on any one of its records (pure): a push
/// makes the data durable only once the barrier follows it.
fn batch_effects(steps: &[BatchStep]) -> PutEffects {
    let mut pushed = false;
    steps
        .iter()
        .fold(PutEffects::default(), |mut effects, step| {
            match step {
                BatchStep::WriteTemporaries => effects.data = true,
                BatchStep::PushTemporaries => pushed = effects.data,
                BatchStep::Barrier => effects.data_durable = pushed,
                BatchStep::Renames => effects.renamed = true,
                BatchStep::SyncDirectory => effects.rename_durable = effects.renamed,
                BatchStep::Witness => effects.marked = true,
            }
            effects
        })
}

/// Every outcome a crash after `effects` may leave: each unflushed effect survives or not.
fn put_outcomes(effects: PutEffects) -> Vec<Outcome> {
    let survives = |made: bool, durable: bool| match (made, durable) {
        (_, true) => vec![true],
        (true, false) => vec![false, true],
        (false, false) => vec![false],
    };
    let mut outcomes = Vec::new();
    for rename in survives(effects.renamed, effects.rename_durable) {
        for data in survives(effects.data, effects.data_durable) {
            outcomes.push(match (rename, data) {
                (true, true) => Outcome::New,
                (true, false) => Outcome::Torn,
                (false, _) => Outcome::Old,
            });
        }
    }
    outcomes
}

/// Presence law of a put: at every crash point, a durable marker names a durable rename, so the
/// record it marks survives the crash (pure).
fn marker_names_a_record(effects: PutEffects) -> bool {
    !effects.marked || effects.rename_durable
}

/// Whether a crash after the first `steps` of a removal plan may leave the record gone while
/// its marker survives: the removal outlives the marker's removal only if the marker's removal
/// came first and was flushed (pure).
fn removal_may_orphan_the_marker(steps: &[RemoveStep]) -> bool {
    let unwitnessed = steps.contains(&RemoveStep::Unwitness);
    steps.contains(&RemoveStep::Remove) && !unwitnessed
}

/// Every outcome a crash after the first `steps` of a removal plan may leave.
fn remove_outcomes(steps: &[RemoveStep]) -> Vec<Outcome> {
    let removed = steps.contains(&RemoveStep::Remove);
    // Unwitness flushes the directory itself; only a flush after the removal makes it durable.
    let durable = removed
        && steps
            .iter()
            .skip_while(|step| **step != RemoveStep::Remove)
            .any(|step| *step == RemoveStep::SyncDirectory);
    match (removed, durable) {
        (_, true) => vec![Outcome::Removed],
        (true, false) => vec![Outcome::Old, Outcome::Removed],
        (false, false) => vec![Outcome::Old],
    }
}

/// The plans themselves: dropping or reordering a flush fails here before the model runs.
#[test]
fn test_plans_flush_before_and_after_the_rename() {
    assert_eq!(RecordAuthority::Authoritative.put_plan(), [
        PutStep::WriteTemporary,
        PutStep::SyncTemporary,
        PutStep::Rename,
        PutStep::SyncDirectory,
        PutStep::Witness,
    ]);
    assert_eq!(RecordAuthority::Authoritative.remove_plan(), [
        RemoveStep::Unwitness,
        RemoveStep::Remove,
        RemoveStep::SyncDirectory,
    ]);
    assert_eq!(RecordAuthority::DurableCache.put_plan(), [
        PutStep::WriteTemporary,
        PutStep::SyncTemporary,
        PutStep::Rename,
        PutStep::SyncDirectory,
    ]);
    assert_eq!(RecordAuthority::DurableCache.remove_plan(), [
        RemoveStep::Remove,
        RemoveStep::SyncDirectory,
    ]);
    assert_eq!(RecordAuthority::Disposable.put_plan(), [
        PutStep::WriteTemporary,
        PutStep::Rename,
    ]);
    assert_eq!(RecordAuthority::Disposable.remove_plan(), [
        RemoveStep::Remove
    ]);
}

/// Durability law of an authoritative put and of a durable cache's (#911): at every crash
/// point the record is old or new, never torn, and new once the plan completes.
#[test]
fn test_an_authoritative_put_is_never_torn_by_a_crash() {
    for authority in [
        RecordAuthority::Authoritative,
        RecordAuthority::Retained,
        RecordAuthority::DurableCache,
    ] {
        let plan = authority.put_plan();
        for crash in 0..=plan.len() {
            let outcomes = put_outcomes(put_effects(plan.get(..crash).unwrap_or_default()));
            assert!(
                outcomes
                    .iter()
                    .all(|outcome| matches!(outcome, Outcome::Old | Outcome::New)),
                "{authority:?}: crash after {crash} steps: {outcomes:?}"
            );
        }
        assert_eq!(put_outcomes(put_effects(plan)), [Outcome::New]);
    }
}

/// The model discriminates: a disposable put, which skips the flushes, can tear its record.
#[test]
fn test_a_disposable_put_can_be_torn_by_a_crash() {
    let plan = RecordAuthority::Disposable.put_plan();
    assert!(put_outcomes(put_effects(plan)).contains(&Outcome::Torn));
}

/// Durability law of an authoritative removal: at every crash point the record is old or
/// removed, and removed once the plan completes; an interrupted removal, or a disposable one,
/// may be rolled back.
#[test]
fn test_a_completed_authoritative_removal_is_not_rolled_back() {
    let plan = RecordAuthority::Authoritative.remove_plan();
    for crash in 0..=plan.len() {
        let outcomes = remove_outcomes(plan.get(..crash).unwrap_or_default());
        assert!(
            outcomes
                .iter()
                .all(|outcome| matches!(outcome, Outcome::Old | Outcome::Removed)),
            "crash after {crash} steps: {outcomes:?}"
        );
    }
    assert_eq!(remove_outcomes(plan), [Outcome::Removed]);
    assert!(remove_outcomes(plan.get(..1).unwrap_or_default()).contains(&Outcome::Old));
    assert!(remove_outcomes(RecordAuthority::Disposable.remove_plan()).contains(&Outcome::Old));
}

/// Presence law (#915): at every crash point of an authoritative put, a durable marker names a
/// record that survives; at every crash point of an authoritative removal, the marker is gone
/// before the record is, so no crash leaves a marker without its record.
#[test]
fn test_a_marker_always_names_a_record_at_every_crash_point() {
    let put = RecordAuthority::Authoritative.put_plan();
    for crash in 0..=put.len() {
        let effects = put_effects(put.get(..crash).unwrap_or_default());
        assert!(
            marker_names_a_record(effects),
            "put crash after {crash} steps"
        );
    }
    assert!(put_effects(put).marked);
    let remove = RecordAuthority::Authoritative.remove_plan();
    for crash in 0..=remove.len() {
        assert!(
            !removal_may_orphan_the_marker(remove.get(..crash).unwrap_or_default()),
            "removal crash after {crash} steps"
        );
    }
}

/// The presence model discriminates: a plan that marks before the rename is durable, or
/// removes the record before its marker, breaks the law.
#[test]
fn test_the_presence_model_rejects_a_misordered_plan() {
    let early = put_effects(&[PutStep::WriteTemporary, PutStep::Rename, PutStep::Witness]);
    assert!(!marker_names_a_record(early));
    assert!(removal_may_orphan_the_marker(&[RemoveStep::Remove]));
}

/// The batch plans themselves: dropping or reordering the barrier or a flush fails here first.
#[test]
fn test_batch_plans_barrier_before_the_renames() {
    assert_eq!(RecordAuthority::Authoritative.batch_plan(), [
        BatchStep::WriteTemporaries,
        BatchStep::PushTemporaries,
        BatchStep::Barrier,
        BatchStep::Renames,
        BatchStep::SyncDirectory,
        BatchStep::Witness,
    ]);
    assert_eq!(RecordAuthority::DurableCache.batch_plan(), [
        BatchStep::WriteTemporaries,
        BatchStep::PushTemporaries,
        BatchStep::Barrier,
        BatchStep::Renames,
        BatchStep::SyncDirectory,
    ]);
    assert_eq!(RecordAuthority::Disposable.batch_plan(), [
        BatchStep::WriteTemporaries,
        BatchStep::Renames,
    ]);
}

/// Atomicity per record of a durable batch (#916): at every crash point each record of the
/// batch is old or new, never torn, its marker names a record that survives, and every record
/// is new once the plan completes.
#[test]
fn test_a_durable_batch_never_tears_a_record_at_any_crash_point() {
    for authority in [
        RecordAuthority::Authoritative,
        RecordAuthority::Retained,
        RecordAuthority::DurableCache,
    ] {
        let plan = authority.batch_plan();
        for crash in 0..=plan.len() {
            let effects = batch_effects(plan.get(..crash).unwrap_or_default());
            let outcomes = put_outcomes(effects);
            assert!(
                outcomes
                    .iter()
                    .all(|outcome| matches!(outcome, Outcome::Old | Outcome::New)),
                "{authority:?}: crash after {crash} steps: {outcomes:?}"
            );
            assert!(
                marker_names_a_record(effects),
                "{authority:?}: crash after {crash} steps"
            );
        }
        assert_eq!(put_outcomes(batch_effects(plan)), [Outcome::New]);
    }
}

/// The batch model discriminates: without the barrier a pushed record is not durable on Apple
/// targets, so a crash after the flushed rename can tear it.
#[test]
fn test_a_batch_without_its_barrier_can_be_torn() {
    let unbarriered = batch_effects(&[
        BatchStep::WriteTemporaries,
        BatchStep::PushTemporaries,
        BatchStep::Renames,
        BatchStep::SyncDirectory,
    ]);
    assert!(put_outcomes(unbarriered).contains(&Outcome::Torn));
}
