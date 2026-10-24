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

#[test]
fn test_a_check_answers_within_the_freshness_window_negative_ones_included() {
    let mut memory = memory(8);
    memory.observe::<Unreachable>(TestOwner(1), Ok(Some(serving(25))), 0);
    memory.observe::<Unreachable>(TestOwner(2), Ok(None), 0);
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
    memory.observe::<Unreachable>(TestOwner(1), Ok(Some(serving(25))), 0);
    for unseen in [Ok(None), Err(Unreachable)] {
        let observed = memory.observe(TestOwner(1), unseen, 800);
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
    let expired = memory.observe::<Unreachable>(TestOwner(1), Ok(None), 801);
    assert_eq!(expired, Observation {
        answer: Ok(None),
        changes: vec![MemoryChange::Forget(TestOwner(1))],
    });
}

#[test]
fn test_a_failed_lookup_past_grace_stays_a_failure_and_forgets() {
    let mut memory = memory(8);
    assert_eq!(
        memory.observe(TestOwner(1), Err(Unreachable), 0),
        Observation {
            answer: Err(Unreachable),
            changes: vec![],
        }
    );
    memory.observe::<Unreachable>(TestOwner(2), Ok(Some(serving(25))), 0);
    assert_eq!(
        memory.observe(TestOwner(2), Err(Unreachable), 900),
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
    memory.observe::<Unreachable>(TestOwner(1), Ok(Some(serving(25))), 0);
    let withdrawn =
        memory.observe::<Unreachable>(TestOwner(1), Ok(Some(Announcement::default())), 1);
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
        memory
            .observe::<Unreachable>(TestOwner(1), Ok(Some(serving(port))), at)
            .changes
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
    memory.observe::<Unreachable>(TestOwner(1), Ok(Some(serving(25))), 0);
    memory.observe::<Unreachable>(TestOwner(2), Ok(None), 5);
    // At 6, nothing is past its window: the absent owner 2 goes before the present owner 1.
    let observed = memory.observe::<Unreachable>(TestOwner(3), Ok(Some(serving(22))), 6);
    assert_eq!(
        observed.changes,
        vec![keep(3, 22, 6)],
        "an absent eviction persists nothing"
    );
    assert_eq!(memory.recall(&TestOwner(2), 6), Recalled::Stale);
    // At 7, both held owners are present: the one checked longest ago, owner 1, goes.
    let observed = memory.observe::<Unreachable>(TestOwner(4), Ok(None), 7);
    assert_eq!(observed.changes, vec![MemoryChange::Forget(TestOwner(1))]);
    assert_eq!(memory.len(), 2);
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
