use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Mutex as SyncMutex;

use tokio::sync::Notify;

use super::*;
use crate::owner::test_owner::TestOwner;

/// The directory failure of the cases.
#[derive(Debug, Error)]
#[error("unreachable")]
struct Unreachable;

/// A directory whose answers the test sets, counting its lookups. A lookup of the held owner
/// signals `entered`, then waits for `release`.
#[derive(Default)]
struct Settable {
    answers: SyncMutex<BTreeMap<TestOwner, Announcement>>,
    offline: AtomicBool,
    lookups: AtomicUsize,
    held: SyncMutex<Option<TestOwner>>,
    entered: Notify,
    release: Notify,
}

impl Settable {
    /// Make `owner` announce `announcement`, or nothing when `None`.
    fn set(&self, owner: TestOwner, announcement: Option<Announcement>) {
        let mut answers = self.answers.lock().unwrap();
        match announcement {
            Some(announcement) => answers.insert(owner, announcement),
            None => answers.remove(&owner),
        };
    }

    /// Make every lookup fail with [`Unreachable`] when `offline` is `true`.
    fn set_offline(&self, offline: bool) {
        self.offline.store(offline, Ordering::SeqCst);
    }
}

#[async_trait]
impl Directory for Settable {
    type Error = Unreachable;
    type Owner = TestOwner;

    async fn announcement(&self, owner: TestOwner) -> Result<Option<Announcement>, Unreachable> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        let held = *self.held.lock().unwrap() == Some(owner);
        if held {
            self.entered.notify_one();
            self.release.notified().await;
        }
        if self.offline.load(Ordering::SeqCst) {
            return Err(Unreachable);
        }
        Ok(self.answers.lock().unwrap().get(&owner).cloned())
    }
}

/// An owner receiving mail, with a TXT record.
fn mail() -> Announcement {
    Announcement::new([(25, ProxyProtocol::V2)], [(
        SubName::new("_acme-challenge").unwrap(),
        vec![TxtValue::new("token").unwrap()],
    )])
    .unwrap()
}

/// Windows fresh for an hour, so a second use within a case is fresh.
const FRESH: MemoryWindows = MemoryWindows {
    fresh_ms: 3_600_000,
    grace_ms: 7_200_000,
};

/// Windows fresh for nothing, so every use asks the directory.
const STALE: MemoryWindows = MemoryWindows {
    fresh_ms: 0,
    grace_ms: 7_200_000,
};

/// A recall over `directory`, kept in `dir`, with `max_lookups` lookups in flight.
async fn open(
    directory: &Arc<Settable>,
    dir: &Path,
    windows: MemoryWindows,
    max_lookups: usize,
) -> Recall<Settable> {
    Recall::open(Arc::clone(directory), dir, windows, 64, max_lookups)
        .await
        .unwrap()
}

#[tokio::test]
async fn test_a_fresh_answer_spares_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let directory = Arc::new(Settable::default());
    directory.set(TestOwner(1), Some(mail()));
    let recall = open(&directory, dir.path(), FRESH, 4).await;
    for _ in 0..2 {
        assert_eq!(
            recall.announcement(TestOwner(1)).await.unwrap(),
            Some(mail())
        );
        assert_eq!(recall.announcement(TestOwner(2)).await.unwrap(), None);
    }
    assert_eq!(
        directory.lookups.load(Ordering::SeqCst),
        2,
        "one lookup per owner"
    );
}

#[tokio::test]
async fn test_a_spent_budget_answers_from_grace_or_refuses_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let directory = Arc::new(Settable::default());
    directory.set(TestOwner(1), Some(mail()));
    let recall = Arc::new(open(&directory, dir.path(), STALE, 1).await);
    assert_eq!(
        recall.announcement(TestOwner(1)).await.unwrap(),
        Some(mail())
    );
    // Hold the only slot with a lookup of owner 2.
    *directory.held.lock().unwrap() = Some(TestOwner(2));
    let holder = tokio::spawn({
        let recall = Arc::clone(&recall);
        async move { recall.announcement(TestOwner(2)).await }
    });
    directory.entered.notified().await;
    assert_eq!(
        recall.announcement(TestOwner(1)).await.unwrap(),
        Some(mail()),
        "the remembered announcement answers while the slot is taken"
    );
    assert!(matches!(
        recall.announcement(TestOwner(3)).await,
        Err(RecallError::Busy)
    ));
    assert_eq!(
        directory.lookups.load(Ordering::SeqCst),
        2,
        "neither waited for a slot"
    );
    directory.release.notify_one();
    assert_eq!(holder.await.unwrap().unwrap(), None);
}

#[tokio::test]
async fn test_a_vanished_owner_is_remembered_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let directory = Arc::new(Settable::default());
    directory.set(TestOwner(1), Some(mail()));
    let recall = open(&directory, dir.path(), FRESH, 4).await;
    assert_eq!(
        recall.announcement(TestOwner(1)).await.unwrap(),
        Some(mail())
    );
    drop(recall);
    directory.set(TestOwner(1), None);
    let restarted = open(&directory, dir.path(), FRESH, 4).await;
    assert_eq!(
        restarted.announcement(TestOwner(1)).await.unwrap(),
        Some(mail())
    );
}

#[tokio::test]
async fn test_a_withdrawal_is_forgotten_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let directory = Arc::new(Settable::default());
    directory.set(TestOwner(1), Some(mail()));
    let recall = open(&directory, dir.path(), STALE, 4).await;
    recall.announcement(TestOwner(1)).await.unwrap();
    directory.set(TestOwner(1), Some(Announcement::default()));
    assert_eq!(recall.announcement(TestOwner(1)).await.unwrap(), None);
    drop(recall);
    directory.set(TestOwner(1), None);
    let restarted = open(&directory, dir.path(), STALE, 4).await;
    assert_eq!(restarted.announcement(TestOwner(1)).await.unwrap(), None);
}

#[tokio::test]
async fn test_a_malformed_journal_record_is_never_answered() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(MEMORY_FILE),
        "{\"Put\":[\"o1\",{\"ports\":[[0,\"off\"]],\"txt\":[],\"seen_at_ms\":0}]}\n\
         {\"Put\":[\"not an owner\",{\"ports\":[[25,\"off\"]],\"txt\":[],\"seen_at_ms\":0}]}\n",
    )
    .unwrap();
    let directory = Arc::new(Settable::default());
    let recall = open(&directory, dir.path(), FRESH, 4).await;
    assert_eq!(recall.announcement(TestOwner(1)).await.unwrap(), None);
    assert_eq!(
        directory.lookups.load(Ordering::SeqCst),
        1,
        "nothing was restored"
    );
}

#[tokio::test]
async fn test_a_failed_withdrawal_journal_remove_is_retried_and_not_resurrected_on_restart() {
    let short_fresh = MemoryWindows {
        fresh_ms: 15,
        grace_ms: 7_200_000,
    };
    for windows in [STALE, short_fresh] {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join(MEMORY_FILE);
        let directory = Arc::new(Settable::default());
        directory.set(TestOwner(1), Some(mail()));
        directory.set(TestOwner(2), Some(mail()));

        // 1. Persist non-empty announcements for owner 1 (to withdraw) and owner 2 (control).
        let recall = open(&directory, dir.path(), windows, 4).await;
        assert_eq!(
            recall.announcement(TestOwner(1)).await.unwrap(),
            Some(mail())
        );
        assert_eq!(
            recall.announcement(TestOwner(2)).await.unwrap(),
            Some(mail())
        );

        if windows.fresh_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(windows.fresh_ms + 10)).await;
        }

        // 2 & 3. Observe an explicit empty announcement while journal writes fail (`EISDIR`).
        directory.set(TestOwner(1), Some(Announcement::default()));
        let saved_journal = std::fs::read(&journal_path).unwrap();
        std::fs::remove_file(&journal_path).unwrap();
        std::fs::create_dir(&journal_path).unwrap();

        let lookups_before_failed_withdraw = directory.lookups.load(Ordering::SeqCst);
        assert_eq!(
            recall.announcement(TestOwner(1)).await.unwrap(),
            None,
            "best-effort cache policy still answers the withdrawal at once"
        );
        assert_eq!(
            directory.lookups.load(Ordering::SeqCst),
            lookups_before_failed_withdraw + 1
        );

        if windows.fresh_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(windows.fresh_ms + 10)).await;
        }

        // 4. Restore journal IO (still holding owner 1's old Present record on disk) and refresh
        // the unchanged withdrawal from the directory.
        std::fs::remove_dir(&journal_path).unwrap();
        std::fs::write(&journal_path, &saved_journal).unwrap();
        let lookups_before_retry = directory.lookups.load(Ordering::SeqCst);
        assert_eq!(recall.announcement(TestOwner(1)).await.unwrap(), None);
        assert_eq!(
            directory.lookups.load(Ordering::SeqCst),
            lookups_before_retry + 1,
            "directory refresh observed unchanged withdrawal"
        );
        drop(recall);

        // 5. Restart within the grace window with an unavailable directory: owner 2 falls back to
        // grace memory, while withdrawn owner 1 must not resurrect its stale announcement.
        if windows.fresh_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(windows.fresh_ms + 10)).await;
        }
        directory.set_offline(true);
        let restarted = open(&directory, dir.path(), windows, 4).await;
        assert_eq!(
            restarted.announcement(TestOwner(2)).await.unwrap(),
            Some(mail()),
            "grace fallback is active while the directory is unavailable"
        );
        assert!(
            matches!(
                restarted.announcement(TestOwner(1)).await,
                Err(RecallError::Directory(Unreachable))
            ),
            "withdrawn owner must not be served from grace memory after restart"
        );
    }
}

#[tokio::test]
async fn test_a_fresh_hit_flushes_a_pending_withdrawal_once_io_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join(MEMORY_FILE);
    let windows = MemoryWindows {
        fresh_ms: 60_000,
        grace_ms: 7_200_000,
    };
    let stale_seen = now_ms().saturating_sub(windows.fresh_ms + 1_000);
    std::fs::write(
        &journal_path,
        format!("{{\"Put\":[\"o1\",{{\"ports\":[[25,\"v2\"]],\"txt\":[],\"seen_at_ms\":{stale_seen}}}]}}\n"),
    )
    .unwrap();

    let directory = Arc::new(Settable::default());
    directory.set(TestOwner(1), Some(Announcement::default()));
    let recall = open(&directory, dir.path(), windows, 4).await;

    // Break journal IO before the stale lookup observes the withdrawal.
    let saved_journal = std::fs::read(&journal_path).unwrap();
    std::fs::remove_file(&journal_path).unwrap();
    std::fs::create_dir(&journal_path).unwrap();

    assert_eq!(recall.announcement(TestOwner(1)).await.unwrap(), None);
    assert_eq!(directory.lookups.load(Ordering::SeqCst), 1);

    // Restore journal IO and ask again within the 60s freshness window: no directory lookup runs,
    // yet the pending `Forget` is flushed.
    std::fs::remove_dir(&journal_path).unwrap();
    std::fs::write(&journal_path, &saved_journal).unwrap();
    assert_eq!(recall.announcement(TestOwner(1)).await.unwrap(), None);
    assert_eq!(
        directory.lookups.load(Ordering::SeqCst),
        1,
        "fresh recall spared the directory while flushing pending Forget"
    );
    drop(recall);

    directory.set_offline(true);
    let restarted = open(&directory, dir.path(), STALE, 4).await;
    assert!(matches!(
        restarted.announcement(TestOwner(1)).await,
        Err(RecallError::Directory(Unreachable))
    ));
}
