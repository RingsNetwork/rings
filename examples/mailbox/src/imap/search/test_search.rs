use std::collections::BTreeSet;

use imap_next::imap_types::core::Vec1;
use imap_next::imap_types::sequence::SequenceSet;

use super::*;

/// A message with UID `uid`, flags `flags`, received at `date`, of `size` bytes.
fn message(uid: u32, flags: &[Flag], date: i64, size: u32) -> Message {
    Message {
        uid: NonZeroU32::new(uid).unwrap(),
        flags: flags.iter().cloned().collect::<BTreeSet<_>>(),
        internal_date: date,
        size,
    }
}

/// Whether `key` matches message `seq` of a two-message view, with `content`.
fn check(key: SearchKey<'_>, message: &Message, seq: u32, content: Option<&Content>) -> bool {
    let candidate = Candidate {
        seq: NonZeroU32::new(seq).unwrap(),
        message,
        largest: (NonZeroU32::new(2).unwrap(), NonZeroU32::new(9).unwrap()),
        content,
    };
    matches(&key, &candidate)
}

#[test]
fn test_flag_keys_and_their_negations_partition_messages() {
    let seen = message(1, &[Flag::Seen], 0, 10);
    let unseen = message(2, &[], 0, 10);
    for (key, opposite) in [
        (SearchKey::Seen, SearchKey::Unseen),
        (SearchKey::Flagged, SearchKey::Unflagged),
        (SearchKey::Deleted, SearchKey::Undeleted),
    ] {
        for message in [&seen, &unseen] {
            assert_ne!(
                check(key.clone(), message, 1, None),
                check(opposite.clone(), message, 1, None)
            );
        }
    }
    assert!(check(SearchKey::Seen, &seen, 1, None));
    assert!(check(
        SearchKey::Not(Box::new(SearchKey::Seen)),
        &unseen,
        2,
        None
    ));
}

#[test]
fn test_sets_sizes_and_dates_match() {
    let message = message(9, &[], 86_400 * 3, 500);
    let set = |text: &str| SequenceSet::try_from(text).unwrap();
    assert!(check(SearchKey::SequenceSet(set("2")), &message, 2, None));
    assert!(
        check(SearchKey::Uid(set("*")), &message, 2, None),
        "* is the largest UID"
    );
    assert!(check(SearchKey::Larger(499), &message, 2, None));
    assert!(!check(SearchKey::Smaller(500), &message, 2, None));
    let day = |d: u32| {
        imap_next::imap_types::datetime::NaiveDate::try_from(
            chrono::NaiveDate::from_ymd_opt(1970, 1, d).unwrap(),
        )
        .unwrap()
    };
    assert!(check(SearchKey::On(day(4)), &message, 2, None));
    assert!(check(SearchKey::Since(day(4)), &message, 2, None));
    assert!(check(SearchKey::Before(day(5)), &message, 2, None));
    assert!(!check(SearchKey::Before(day(4)), &message, 2, None));
}

#[test]
fn test_text_keys_read_the_content_case_insensitively() {
    let content = Content::of(
        b"From: Alice <a@x>\r\nSubject: Quarterly\r\n Report\r\n\r\nThe numbers are in.",
    );
    let message = message(1, &[], 0, 10);
    let text =
        |value: &str| imap_next::imap_types::core::AString::try_from(value.to_owned()).unwrap();
    assert!(
        check(
            SearchKey::Subject(text("REPORT")),
            &message,
            1,
            Some(&content)
        ),
        "folded"
    );
    assert!(check(
        SearchKey::From(text("alice")),
        &message,
        1,
        Some(&content)
    ));
    assert!(check(
        SearchKey::Body(text("NUMBERS")),
        &message,
        1,
        Some(&content)
    ));
    assert!(!check(
        SearchKey::Body(text("alice")),
        &message,
        1,
        Some(&content)
    ));
    assert!(check(
        SearchKey::Text(text("alice")),
        &message,
        1,
        Some(&content)
    ));
    assert!(!check(
        SearchKey::To(text("alice")),
        &message,
        1,
        Some(&content)
    ));
}

#[test]
fn test_needs_content_finds_text_keys_anywhere() {
    let subject = SearchKey::Subject(imap_next::imap_types::core::AString::try_from("x").unwrap());
    assert!(!needs_content(&SearchKey::Seen));
    assert!(needs_content(&SearchKey::Not(Box::new(subject.clone()))));
    assert!(needs_content(&SearchKey::And(
        Vec1::try_from(vec![SearchKey::Seen, subject]).unwrap()
    )));
}
