use super::*;
use crate::announcement::ProxyProtocol;
use crate::owner::test_owner::TestOwner;

/// The windows of every case: fresh for 10, graced for 800.
const WINDOWS: MemoryWindows = MemoryWindows {
    fresh_ms: 10,
    grace_ms: 800,
};

/// The directory failure of every case.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Unreachable;

/// An owner serving `port`.
fn serving(port: u16) -> Announcement {
    Announcement::new([(port, ProxyProtocol::Off)], []).unwrap()
}

/// A memory of every case's windows, holding at most `capacity` owners.
fn memory(capacity: usize) -> AnnouncementMemory<TestOwner> {
    AnnouncementMemory::new(WINDOWS, capacity)
}

/// The change keeping owner `n`'s announcement serving `port`, seen at `at`.
fn keep(n: u32, port: u16, at: u64) -> MemoryChange<TestOwner> {
    MemoryChange::Keep {
        owner: TestOwner(n),
        announcement: serving(port),
        seen_at_ms: at,
    }
}

/// Observe `observed` for `owner` at `now_ms` and mark every emitted change persisted.
fn observe_persisted<E>(
    memory: &mut AnnouncementMemory<TestOwner>,
    owner: TestOwner,
    observed: Result<Option<Announcement>, E>,
    now_ms: u64,
) -> Observation<TestOwner, E> {
    let observation = memory.observe(owner, observed, now_ms);
    for change in &observation.changes {
        memory.mark_persisted(change);
    }
    observation
}

#[test]
fn test_a_check_answers_within_the_freshness_window_negative_ones_included() {
    let mut memory = memory(8);
    observe_persisted::<Unreachable>(&mut memory, TestOwner(1), Ok(Some(serving(25))), 0);
    observe_persisted::<Unreachable>(&mut memory, TestOwner(2), Ok(None), 0);
    assert_eq!(
        memory.recall(&TestOwner(1), 9),
        Recalled::Fresh(Some(serving(25)))
    );
    assert_eq!(memory.recall(&TestOwner(2), 9), Recalled::Fresh(None));
    assert_eq!(memory.recall(&TestOwner(1), 10), Recalled::Stale);
    assert_eq!(
        memory.recall(&TestOwner(3), 0),
        Recalled::Stale,
        "never asked"
    );
}

#[test]
fn test_a_vanished_owner_is_answered_from_memory_within_grace_only() {
    let mut memory = memory(8);
    observe_persisted::<Unreachable>(&mut memory, TestOwner(1), Ok(Some(serving(25))), 0);
    for unseen in [Ok(None), Err(Unreachable)] {
        let observed = observe_persisted(&mut memory, TestOwner(1), unseen, 800);
        assert_eq!(observed, Observation {
            answer: Ok(Some(serving(25))),
            changes: vec![],
        });
    }
    assert_eq!(
        memory.recall(&TestOwner(1), 805),
        Recalled::Fresh(Some(serving(25))),
        "a graced answer was a check too"
    );
    let expired = observe_persisted::<Unreachable>(&mut memory, TestOwner(1), Ok(None), 801);
    assert_eq!(expired, Observation {
        answer: Ok(None),
        changes: vec![MemoryChange::Forget(TestOwner(1))],
    });
}

#[test]
fn test_a_failed_lookup_past_grace_stays_a_failure_and_forgets() {
    let mut memory = memory(8);
    assert_eq!(
        observe_persisted(&mut memory, TestOwner(1), Err(Unreachable), 0),
        Observation {
            answer: Err(Unreachable),
            changes: vec![],
        }
    );
    observe_persisted::<Unreachable>(&mut memory, TestOwner(2), Ok(Some(serving(25))), 0);
    assert_eq!(
        observe_persisted(&mut memory, TestOwner(2), Err(Unreachable), 900),
        Observation {
            answer: Err(Unreachable),
            changes: vec![MemoryChange::Forget(TestOwner(2))],
        }
    );
    assert_eq!(memory.graced(&TestOwner(2), 900), None);
}

#[test]
fn test_an_empty_announcement_withdraws_at_once() {
    let mut memory = memory(8);
    observe_persisted::<Unreachable>(&mut memory, TestOwner(1), Ok(Some(serving(25))), 0);
    let withdrawn = observe_persisted::<Unreachable>(
        &mut memory,
        TestOwner(1),
        Ok(Some(Announcement::default())),
        1,
    );
    assert_eq!(withdrawn, Observation {
        answer: Ok(None),
        changes: vec![MemoryChange::Forget(TestOwner(1))],
    });
    assert_eq!(
        memory.graced(&TestOwner(1), 2),
        None,
        "nothing is graced after a withdrawal"
    );
}

#[test]
fn test_a_sighting_is_persisted_on_change_and_past_the_refresh_only() {
    // Grace 800: a sighting is persisted again once it moved more than 100.
    let mut memory = memory(8);
    let changes = |memory: &mut AnnouncementMemory<TestOwner>, port, at| {
        observe_persisted::<Unreachable>(memory, TestOwner(1), Ok(Some(serving(port))), at).changes
    };
    assert_eq!(changes(&mut memory, 25, 0), vec![keep(1, 25, 0)]);
    assert_eq!(changes(&mut memory, 25, 100), vec![]);
    assert_eq!(changes(&mut memory, 25, 101), vec![keep(1, 25, 101)]);
    assert_eq!(
        changes(&mut memory, 22, 102),
        vec![keep(1, 22, 102)],
        "a change persists at once"
    );
}

#[test]
fn test_a_full_memory_forgets_the_expired_then_the_oldest_absent_first() {
    let mut memory = memory(2);
    observe_persisted::<Unreachable>(&mut memory, TestOwner(1), Ok(Some(serving(25))), 0);
    observe_persisted::<Unreachable>(&mut memory, TestOwner(2), Ok(None), 5);
    // At 6, nothing is past its window: the absent owner 2 goes before the present owner 1.
    let observed =
        observe_persisted::<Unreachable>(&mut memory, TestOwner(3), Ok(Some(serving(22))), 6);
    assert_eq!(
        observed.changes,
        vec![keep(3, 22, 6)],
        "an absent eviction persists nothing"
    );
    assert_eq!(memory.recall(&TestOwner(2), 6), Recalled::Stale);
    // At 7, both held owners are present: the one checked longest ago, owner 1, goes.
    let observed = observe_persisted::<Unreachable>(&mut memory, TestOwner(4), Ok(None), 7);
    assert_eq!(observed.changes, vec![MemoryChange::Forget(TestOwner(1))]);
    assert_eq!(memory.len(), 2);
}

#[test]
fn test_an_unpersisted_withdrawal_stays_pending_across_unchanged_refreshes() {
    let mut memory = memory(8);
    observe_persisted::<Unreachable>(&mut memory, TestOwner(1), Ok(Some(serving(25))), 0);

    // First withdrawal: journal removal fails, so `mark_persisted` is not called.
    let first = memory.observe::<Unreachable>(TestOwner(1), Ok(Some(Announcement::default())), 10);
    assert_eq!(first, Observation {
        answer: Ok(None),
        changes: vec![MemoryChange::Forget(TestOwner(1))],
    });
    assert!(memory.has_pending_changes());

    // Repeated unchanged withdrawal and vanished refresh still emit `Forget` until persisted.
    let second = memory.observe::<Unreachable>(TestOwner(1), Ok(Some(Announcement::default())), 20);
    assert_eq!(second, Observation {
        answer: Ok(None),
        changes: vec![MemoryChange::Forget(TestOwner(1))],
    });
    let third = memory.observe::<Unreachable>(TestOwner(1), Ok(None), 30);
    assert_eq!(third, Observation {
        answer: Ok(None),
        changes: vec![MemoryChange::Forget(TestOwner(1))],
    });

    memory.mark_persisted(&MemoryChange::Forget(TestOwner(1)));
    assert!(!memory.has_pending_changes());
    let fourth = memory.observe::<Unreachable>(TestOwner(1), Ok(Some(Announcement::default())), 40);
    assert_eq!(fourth, Observation {
        answer: Ok(None),
        changes: vec![],
    });
}

#[test]
fn test_unpersisted_keeps_and_evictions_retry_until_persisted() {
    let mut memory = memory(1);
    // Unpersisted Keep retries even within the refresh window.
    let first = memory.observe::<Unreachable>(TestOwner(1), Ok(Some(serving(25))), 0);
    assert_eq!(first.changes, vec![keep(1, 25, 0)]);
    let retry = memory.observe::<Unreachable>(TestOwner(1), Ok(Some(serving(25))), 5);
    assert_eq!(retry.changes, vec![keep(1, 25, 5)]);
    memory.mark_persisted(&keep(1, 25, 5));
    assert!(!memory.has_pending_changes());

    // Evicting persisted owner 1 for owner 2 retains `Forget(1)` until persisted.
    let evicted = memory.observe::<Unreachable>(TestOwner(2), Ok(Some(serving(22))), 10);
    assert_eq!(evicted.changes, vec![
        MemoryChange::Forget(TestOwner(1)),
        keep(2, 22, 10),
    ]);
    // Owner 2 withdraws before anything is persisted: owner 2 cancels cleanly, while `Forget(1)`
    // remains owed to disk.
    let withdrawn =
        memory.observe::<Unreachable>(TestOwner(2), Ok(Some(Announcement::default())), 15);
    assert_eq!(withdrawn.changes, vec![MemoryChange::Forget(TestOwner(1))]);
    memory.mark_persisted(&MemoryChange::Forget(TestOwner(1)));
    assert!(!memory.has_pending_changes());
}

#[test]
fn test_restore_drops_empty_announcements_and_asks_again() {
    let memory = AnnouncementMemory::restore(WINDOWS, 8, [
        (TestOwner(1), serving(25), 5),
        (TestOwner(2), Announcement::default(), 5),
    ]);
    assert_eq!(
        memory
            .remembered()
            .map(|(owner, _, _)| owner)
            .collect::<Vec<_>>(),
        vec![TestOwner(1)]
    );
    assert_eq!(memory.recall(&TestOwner(1), 100), Recalled::Stale);
    assert_eq!(memory.graced(&TestOwner(1), 100), Some(serving(25)));
}

#[test]
fn test_restore_beyond_the_capacity_keeps_the_most_recently_seen() {
    let memory = AnnouncementMemory::restore(WINDOWS, 2, [
        (TestOwner(1), serving(25), 30),
        (TestOwner(2), serving(25), 10),
        (TestOwner(3), serving(25), 20),
    ]);
    let kept = memory
        .remembered()
        .map(|(owner, _, _)| owner)
        .collect::<Vec<_>>();
    assert_eq!(kept, vec![TestOwner(1), TestOwner(3)]);
}
