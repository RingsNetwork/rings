//! The laws of the store reports a node awaits (#866): a receipt resolves at the first report
//! that decides its quorum, or at the last report, with that report's outcome; unknown reports
//! change nothing, and the table stays bounded.

use std::collections::BTreeSet;

use futures::FutureExt;

use super::StoreQuorum;
use super::StoreReports;
use super::StoreUnconfirmed;
use super::MAX_PENDING_STORE_REPORTS;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::Did;
use crate::message::OperationDigest;
use crate::message::StoreOutcome;

/// The digest of a distinct operation, numbered `n`.
fn digest(n: u32) -> OperationDigest {
    let entry: Entry = (format!("topic-{n}"), bytes::Bytes::from_static(b"x"))
        .try_into()
        .expect("a data entry");
    OperationDigest::of(&EntryOperation::Extend(entry)).expect("a digest")
}

/// The placements `0..n` as DIDs.
fn placements(n: u32) -> BTreeSet<Did> {
    (0..n).map(Did::from).collect()
}

/// An operation with no remote placement is stored once sent.
#[test]
fn test_an_operation_with_no_remote_placement_resolves_at_once() {
    let reports = StoreReports::default();
    let receipt = reports.expect(digest(1), BTreeSet::new(), StoreQuorum::Every);
    assert_eq!(receipt.now_or_never(), Some(Ok(())));
    assert_eq!(reports.len(), 0);
}

/// Under [`StoreQuorum::Every`], the receipt waits for every awaited placement, ignores what is
/// not awaited, and resolves once the last reports it stored, leaving the table.
#[test]
fn test_a_receipt_resolves_once_every_placement_reported_stored() {
    let reports = StoreReports::default();
    let operation = digest(1);
    let mut receipt = reports.expect(operation, placements(2), StoreQuorum::Every);

    reports.report(operation, Did::from(0_u32), StoreOutcome::Stored);
    reports.report(operation, Did::from(0_u32), StoreOutcome::Stored);
    reports.report(operation, Did::from(9_u32), StoreOutcome::Stored);
    reports.report(digest(2), Did::from(1_u32), StoreOutcome::Stored);
    assert_eq!(
        (&mut receipt).now_or_never(),
        None,
        "one placement is unreported"
    );

    reports.report(operation, Did::from(1_u32), StoreOutcome::Stored);
    assert_eq!(receipt.now_or_never(), Some(Ok(())));
    assert_eq!(reports.len(), 0);
}

/// Under [`StoreQuorum::Every`], a refusal by any awaited owner resolves the receipt refused,
/// naming the placement.
#[test]
fn test_a_refusal_resolves_the_receipt_refused() {
    let reports = StoreReports::default();
    let operation = digest(1);
    let receipt = reports.expect(operation, placements(2), StoreQuorum::Every);

    reports.report(operation, Did::from(0_u32), StoreOutcome::Stored);
    reports.report(operation, Did::from(1_u32), StoreOutcome::Refused);

    assert_eq!(
        receipt.now_or_never(),
        Some(Err(StoreUnconfirmed::Refused {
            placement: Did::from(1_u32)
        }))
    );
    assert_eq!(reports.len(), 0);
}

/// The bounded law: a full table tracks nothing more, until receipts dropped free it.
#[test]
fn test_the_table_is_bounded_and_dropped_receipts_free_it() {
    let reports = StoreReports::default();
    let capacity = u32::try_from(MAX_PENDING_STORE_REPORTS).expect("a small bound");
    let held = (0..capacity)
        .map(|n| reports.expect(digest(n), placements(1), StoreQuorum::Every))
        .collect::<Vec<_>>();

    let untracked = reports.expect(digest(capacity), placements(1), StoreQuorum::Every);
    assert_eq!(
        untracked.now_or_never(),
        Some(Err(StoreUnconfirmed::Untracked))
    );
    assert_eq!(reports.len(), MAX_PENDING_STORE_REPORTS);

    drop(held);
    let mut tracked = reports.expect(digest(capacity), placements(1), StoreQuorum::Every);
    assert_eq!((&mut tracked).now_or_never(), None);
    assert_eq!(reports.len(), 1);
}

/// A receipt whose table is gone resolves abandoned.
#[test]
fn test_a_receipt_outliving_its_table_is_abandoned() {
    let reports = StoreReports::default();
    let receipt = reports.expect(digest(1), placements(1), StoreQuorum::Every);
    drop(reports);
    assert_eq!(
        receipt.now_or_never(),
        Some(Err(StoreUnconfirmed::Abandoned))
    );
}

/// Under [`StoreQuorum::Any`], a refusal leaves the receipt waiting on the other placements, and
/// the first store resolves it stored, leaving the table: one owner refusing while another
/// stores is stored.
#[test]
fn test_any_quorum_resolves_stored_at_the_first_store_after_a_refusal() {
    let reports = StoreReports::default();
    let operation = digest(1);
    let mut receipt = reports.expect(operation, placements(3), StoreQuorum::Any);

    reports.report(operation, Did::from(0_u32), StoreOutcome::Refused);
    assert_eq!(
        (&mut receipt).now_or_never(),
        None,
        "two placements are unreported"
    );

    reports.report(operation, Did::from(1_u32), StoreOutcome::Stored);
    assert_eq!(receipt.now_or_never(), Some(Ok(())));
    assert_eq!(reports.len(), 0, "the third placement is no longer awaited");
}

/// Under [`StoreQuorum::Any`], the receipt resolves refused only once every placement refused,
/// naming the last.
#[test]
fn test_any_quorum_resolves_refused_once_every_placement_refused() {
    let reports = StoreReports::default();
    let operation = digest(1);
    let mut receipt = reports.expect(operation, placements(2), StoreQuorum::Any);

    reports.report(operation, Did::from(1_u32), StoreOutcome::Refused);
    reports.report(operation, Did::from(1_u32), StoreOutcome::Refused);
    assert_eq!(
        (&mut receipt).now_or_never(),
        None,
        "a repeated refusal is one placement's"
    );

    reports.report(operation, Did::from(0_u32), StoreOutcome::Refused);
    assert_eq!(
        receipt.now_or_never(),
        Some(Err(StoreUnconfirmed::Refused {
            placement: Did::from(0_u32)
        }))
    );
    assert_eq!(reports.len(), 0);
}

/// The duality law: for each quorum, the outcome that decides it resolves the receipt at once,
/// and the other resolves it only as the last report; each with that report's outcome.
#[test]
fn test_each_quorum_is_decided_by_the_dual_outcome() {
    let resolved = |outcome, placement: u32| {
        Some(match outcome {
            StoreOutcome::Stored => Ok(()),
            StoreOutcome::Refused => Err(StoreUnconfirmed::Refused {
                placement: Did::from(placement),
            }),
        })
    };
    for (quorum, deciding, undeciding) in [
        (
            StoreQuorum::Every,
            StoreOutcome::Refused,
            StoreOutcome::Stored,
        ),
        (
            StoreQuorum::Any,
            StoreOutcome::Stored,
            StoreOutcome::Refused,
        ),
    ] {
        assert!(quorum.decided_by(deciding) && !quorum.decided_by(undeciding));
        let reports = StoreReports::default();
        let operation = digest(1);
        let decided = reports.expect(operation, placements(2), quorum);
        reports.report(operation, Did::from(0_u32), deciding);
        assert_eq!(decided.now_or_never(), resolved(deciding, 0), "{quorum:?}");

        let operation = digest(2);
        let mut undecided = reports.expect(operation, placements(2), quorum);
        reports.report(operation, Did::from(0_u32), undeciding);
        assert!(
            (&mut undecided).now_or_never().is_none(),
            "{quorum:?} waits"
        );
        reports.report(operation, Did::from(1_u32), undeciding);
        assert_eq!(
            undecided.now_or_never(),
            resolved(undeciding, 1),
            "{quorum:?}"
        );
    }
}
