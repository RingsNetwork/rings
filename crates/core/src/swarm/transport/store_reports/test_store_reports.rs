//! The laws of the store reports a node awaits (#866): a receipt resolves once every awaited
//! placement reported the operation stored, a refusal resolves it refused, unknown reports change
//! nothing, and the table stays bounded.

use std::collections::BTreeSet;

use futures::FutureExt;

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
    let receipt = reports.expect(digest(1), BTreeSet::new());
    assert_eq!(receipt.now_or_never(), Some(Ok(())));
    assert_eq!(reports.len(), 0);
}

/// The receipt waits for every awaited placement, ignores what is not awaited, and resolves
/// once the last reports it stored, leaving the table.
#[test]
fn test_a_receipt_resolves_once_every_placement_reported_stored() {
    let reports = StoreReports::default();
    let operation = digest(1);
    let mut receipt = reports.expect(operation, placements(2));

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

/// A refusal by any awaited owner resolves the receipt refused, naming the placement.
#[test]
fn test_a_refusal_resolves_the_receipt_refused() {
    let reports = StoreReports::default();
    let operation = digest(1);
    let receipt = reports.expect(operation, placements(2));

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
        .map(|n| reports.expect(digest(n), placements(1)))
        .collect::<Vec<_>>();

    let untracked = reports.expect(digest(capacity), placements(1));
    assert_eq!(
        untracked.now_or_never(),
        Some(Err(StoreUnconfirmed::Untracked))
    );
    assert_eq!(reports.len(), MAX_PENDING_STORE_REPORTS);

    drop(held);
    let mut tracked = reports.expect(digest(capacity), placements(1));
    assert_eq!((&mut tracked).now_or_never(), None);
    assert_eq!(reports.len(), 1);
}

/// A receipt whose table is gone resolves abandoned.
#[test]
fn test_a_receipt_outliving_its_table_is_abandoned() {
    let reports = StoreReports::default();
    let receipt = reports.expect(digest(1), placements(1));
    drop(reports);
    assert_eq!(
        receipt.now_or_never(),
        Some(Err(StoreUnconfirmed::Abandoned))
    );
}
