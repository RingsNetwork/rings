use std::collections::BTreeSet;

use super::*;
use crate::store::FlagChange;

/// A UID or sequence number.
fn n(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}

/// A mailbox holding `count` unflagged messages.
fn mailbox(count: u32) -> Mailbox {
    let mut mailbox = Mailbox::new(n(1));
    for _ in 0..count {
        mailbox.append(BTreeSet::new(), 0, 1).unwrap();
    }
    mailbox
}

/// The store mailbox id of every case.
fn id() -> MailboxId {
    serde_json::from_str("0").unwrap()
}

#[test]
fn test_resolve_names_messages_by_sequence_number_or_uid() {
    let mut store = mailbox(5);
    let deleted = BTreeSet::from([Flag::Deleted]);
    store.change_flags(&BTreeSet::from([n(2)]), FlagChange::Add, &deleted);
    store.expunge(None);
    let view = View::open(id(), &store, false);
    assert_eq!(view.uids(), [n(1), n(3), n(4), n(5)]);
    let set = |text: &str| SequenceSet::try_from(text).unwrap();
    assert_eq!(view.resolve(&set("2:3"), false), [
        (n(2), n(3)),
        (n(3), n(4))
    ]);
    assert_eq!(
        view.resolve(&set("2:3"), true),
        [(n(2), n(3))],
        "UID 2 is gone"
    );
    assert_eq!(view.resolve(&set("*"), false), [(n(4), n(5))]);
    assert_eq!(view.resolve(&set("4:*"), true), [
        (n(3), n(4)),
        (n(4), n(5))
    ]);
    assert_eq!(view.seq_of(n(4)), Some(n(3)));
    assert_eq!(view.seq_of(n(2)), None);
}

#[test]
fn test_sync_reports_expunges_highest_first_then_flags_then_exists() {
    let mut store = mailbox(4);
    let mut view = View::open(id(), &store, false);
    let deleted = BTreeSet::from([Flag::Deleted]);
    store.change_flags(&BTreeSet::from([n(1), n(3)]), FlagChange::Add, &deleted);
    store.expunge(None);
    store.change_flags(
        &BTreeSet::from([n(4)]),
        FlagChange::Add,
        &BTreeSet::from([Flag::Seen]),
    );
    store.append(BTreeSet::new(), 0, 1).unwrap();
    let changes = view.sync(&store, true);
    // Sequence numbers as the client knows them while it applies each EXPUNGE in turn.
    assert_eq!(changes.expunged, [n(3), n(1)]);
    assert_eq!(changes.flags, [(n(2), n(4), BTreeSet::from([Flag::Seen]))]);
    assert_eq!(changes.exists, Some(3));
    assert_eq!(view.uids(), [n(2), n(4), n(5)]);
    assert_eq!(
        view.sync(&store, true),
        Changes::default(),
        "a second sync is quiet"
    );
}

#[test]
fn test_sync_withholds_expunges_when_not_allowed() {
    let mut store = mailbox(2);
    let mut view = View::open(id(), &store, false);
    let deleted = BTreeSet::from([Flag::Deleted]);
    store.change_flags(&BTreeSet::from([n(1)]), FlagChange::Add, &deleted);
    store.expunge(None);
    let withheld = view.sync(&store, false);
    assert!(withheld.expunged.is_empty());
    assert_eq!(view.count(), 2, "the client still sees the removed message");
    assert_eq!(view.sync(&store, true).expunged, [n(1)]);
    assert_eq!(view.count(), 1);
}
