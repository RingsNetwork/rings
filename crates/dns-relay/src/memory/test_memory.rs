use super::*;
use crate::announcement::ServiceName;
use crate::owner::test_owner::TestOwner;

/// The grace period of every case.
const GRACE: u64 = 800;

/// An owner serving `port` as `service`.
fn serving(port: u16, service: &str) -> Announcement {
    Announcement::new([(port, ServiceName::new(service).unwrap())], []).unwrap()
}

/// A failed lookup.
fn failure() -> LookupFailure {
    LookupFailure("unreachable".to_owned())
}

#[test]
fn test_a_current_announcement_is_answered_and_remembered() {
    let mut memory = AnnouncementMemory::new(GRACE);
    let observed = memory.observe(TestOwner(1), Ok(Some(serving(25, "smtp"))), 0);
    assert_eq!(observed, Observation {
        answer: Ok(Some(serving(25, "smtp"))),
        changed: true,
    });
    assert_eq!(memory.entries().count(), 1);
}

#[test]
fn test_a_vanished_owner_is_answered_from_memory_within_grace_only() {
    let mut memory = AnnouncementMemory::new(GRACE);
    memory.observe(TestOwner(1), Ok(Some(serving(25, "smtp"))), 0);
    for unseen in [Ok(None), Err(failure())] {
        let observed = memory.observe(TestOwner(1), unseen, GRACE);
        assert_eq!(observed, Observation {
            answer: Ok(Some(serving(25, "smtp"))),
            changed: false,
        });
    }
    let expired = memory.observe(TestOwner(1), Ok(None), GRACE + 1);
    assert_eq!(expired, Observation {
        answer: Ok(None),
        changed: true,
    });
    assert_eq!(memory.entries().count(), 0);
}

#[test]
fn test_a_failed_lookup_without_memory_stays_a_failure() {
    let mut memory = AnnouncementMemory::<TestOwner>::new(GRACE);
    let observed = memory.observe(TestOwner(1), Err(failure()), 0);
    assert_eq!(observed, Observation {
        answer: Err(failure()),
        changed: false,
    });
}

#[test]
fn test_an_empty_announcement_withdraws_at_once() {
    let mut memory = AnnouncementMemory::new(GRACE);
    memory.observe(TestOwner(1), Ok(Some(serving(25, "smtp"))), 0);
    let withdrawn = memory.observe(TestOwner(1), Ok(Some(Announcement::default())), 1);
    assert_eq!(withdrawn, Observation {
        answer: Ok(None),
        changed: true,
    });
    let after = memory.observe(TestOwner(1), Ok(None), 2);
    assert_eq!(
        after.answer,
        Ok(None),
        "nothing is remembered after a withdrawal"
    );
}

#[test]
fn test_persisting_is_asked_on_change_and_on_a_stale_sighting_only() {
    let mut memory = AnnouncementMemory::new(GRACE);
    let refresh = memory.refresh_ms();
    assert!(
        memory
            .observe(TestOwner(1), Ok(Some(serving(25, "smtp"))), 0)
            .changed
    );
    // Unpersisted: every sighting asks until the memory is persisted.
    assert!(
        memory
            .observe(TestOwner(1), Ok(Some(serving(25, "smtp"))), 1)
            .changed
    );
    memory.mark_persisted();
    assert!(
        !memory
            .observe(TestOwner(1), Ok(Some(serving(25, "smtp"))), 1 + refresh)
            .changed
    );
    assert!(
        memory
            .observe(TestOwner(1), Ok(Some(serving(25, "smtp"))), 2 + refresh)
            .changed
    );
    memory.mark_persisted();
    assert!(
        memory
            .observe(TestOwner(1), Ok(Some(serving(22, "ssh"))), 3 + refresh)
            .changed
    );
}

#[test]
fn test_forget_expired_keeps_the_owners_within_grace() {
    let mut memory = AnnouncementMemory::new(GRACE);
    memory.observe(TestOwner(1), Ok(Some(serving(25, "smtp"))), 0);
    memory.observe(TestOwner(2), Ok(Some(serving(25, "smtp"))), 100);
    assert!(!memory.forget_expired(GRACE));
    assert!(memory.forget_expired(GRACE + 1));
    assert_eq!(
        memory
            .entries()
            .map(|(owner, _)| *owner)
            .collect::<Vec<_>>(),
        vec![TestOwner(2)]
    );
}

#[test]
fn test_from_entries_drops_empty_announcements() {
    let memory = AnnouncementMemory::from_entries(GRACE, [
        (TestOwner(1), serving(25, "smtp"), 5),
        (TestOwner(2), Announcement::default(), 5),
    ]);
    assert_eq!(
        memory
            .entries()
            .map(|(owner, _)| *owner)
            .collect::<Vec<_>>(),
        vec![TestOwner(1)]
    );
}
