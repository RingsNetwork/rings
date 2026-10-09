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
//! ```
//!
//! The law: an authoritative plan leaves the record old or new at every crash point (never
//! torn), and new (or removed) once the plan completes. A disposable plan, which skips the
//! flushes, can tear a record; that case is asserted too, so the model discriminates.

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

/// Every outcome a crash after the first `steps` of a removal plan may leave.
fn remove_outcomes(steps: &[RemoveStep]) -> Vec<Outcome> {
    let removed = steps.contains(&RemoveStep::Remove);
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
    ]);
    assert_eq!(RecordAuthority::Authoritative.remove_plan(), [
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

/// Durability law of an authoritative put: at every crash point the record is old or new,
/// never torn, and new once the plan completes.
#[test]
fn test_an_authoritative_put_is_never_torn_by_a_crash() {
    let plan = RecordAuthority::Authoritative.put_plan();
    for crash in 0..=plan.len() {
        let outcomes = put_outcomes(put_effects(plan.get(..crash).unwrap_or_default()));
        assert!(
            outcomes
                .iter()
                .all(|outcome| matches!(outcome, Outcome::Old | Outcome::New)),
            "crash after {crash} steps: {outcomes:?}"
        );
    }
    assert_eq!(put_outcomes(put_effects(plan)), [Outcome::New]);
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
