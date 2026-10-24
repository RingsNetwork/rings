use super::*;

/// A UID.
fn uid(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}

/// A mailbox holding messages with UIDs 1 to `n`, every even one `\Deleted`.
fn mailbox(n: u32) -> Mailbox {
    let mut mailbox = Mailbox::new(uid(7));
    for index in 1..=n {
        let flags = if index % 2 == 0 {
            BTreeSet::from([Flag::Deleted])
        } else {
            BTreeSet::new()
        };
        assert_eq!(mailbox.append(flags, 0, 10).unwrap(), uid(index));
    }
    mailbox
}

#[test]
fn test_append_assigns_ascending_uids_never_reused() {
    let mut mailbox = mailbox(4);
    assert_eq!(mailbox.expunge(None), vec![uid(2), uid(4)]);
    assert_eq!(mailbox.append(BTreeSet::new(), 0, 1).unwrap(), uid(5));
    let uids = mailbox.messages.iter().map(|m| m.uid).collect::<Vec<_>>();
    assert_eq!(uids, vec![uid(1), uid(3), uid(5)]);
    assert!(uids.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn test_append_refuses_once_every_uid_is_assigned() {
    let mut mailbox = Mailbox::new(uid(1));
    mailbox.uid_next = NonZeroU32::MAX;
    assert_eq!(mailbox.append(BTreeSet::new(), 0, 1), Err(UidsExhausted));
    assert!(mailbox.messages.is_empty());
}

#[test]
fn test_expunge_only_removes_deleted_messages_in_the_given_set() {
    let mut mailbox = mailbox(6);
    let only = BTreeSet::from([uid(2), uid(3)]);
    assert_eq!(mailbox.expunge(Some(&only)), vec![uid(2)]);
    assert_eq!(mailbox.expunge(None), vec![uid(4), uid(6)]);
    assert_eq!(mailbox.expunge(None), Vec::<NonZeroU32>::new());
}

#[test]
fn test_flag_changes_follow_their_algebra() {
    let a = BTreeSet::from([Flag::Seen]);
    let b = BTreeSet::from([Flag::Seen, Flag::Flagged]);
    assert_eq!(FlagChange::Add.apply(&a, &b), b);
    assert_eq!(
        FlagChange::Remove.apply(&b, &a),
        BTreeSet::from([Flag::Flagged])
    );
    assert_eq!(FlagChange::Replace.apply(&b, &a), a);
    // Law: adding then removing the same flags removes them, whatever was there.
    let added = FlagChange::Add.apply(&a, &b);
    assert!(FlagChange::Remove.apply(&added, &b).is_empty());
}

#[test]
fn test_change_flags_reports_only_messages_that_changed() {
    let mut mailbox = mailbox(3);
    let all = BTreeSet::from([uid(1), uid(2), uid(3)]);
    let deleted = BTreeSet::from([Flag::Deleted]);
    let changed = mailbox.change_flags(&all, FlagChange::Add, &deleted);
    assert_eq!(
        changed.iter().map(|(uid, _)| *uid).collect::<Vec<_>>(),
        vec![uid(1), uid(3)],
        "message 2 was deleted already"
    );
    assert_eq!(
        mailbox.message(uid(2)).map(|m| m.flags.clone()),
        Some(deleted)
    );
    assert_eq!(mailbox.message(uid(9)), None);
}

#[test]
fn test_unseen_counts_messages_without_seen() {
    let mut mailbox = mailbox(3);
    assert_eq!(mailbox.unseen(), 3);
    mailbox.change_flags(
        &BTreeSet::from([uid(1)]),
        FlagChange::Add,
        &BTreeSet::from([Flag::Seen]),
    );
    assert_eq!(mailbox.unseen(), 2);
}
