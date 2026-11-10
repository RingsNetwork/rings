//! Observability guard for async locks held across storage or network I/O.

use std::future::Future;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::Instant;

/// Default threshold above which waiting for or holding an async lock logs a warning and
/// increments the process-wide slow-lock counters.
pub const SLOW_ASYNC_LOCK_THRESHOLD: Duration = Duration::from_millis(500);

static SLOW_LOCK_HOLD_COUNT: AtomicU64 = AtomicU64::new(0);
static SLOW_LOCK_WAIT_COUNT: AtomicU64 = AtomicU64::new(0);

/// Number of completed async lock holds that reached or exceeded their slow-hold threshold.
pub fn slow_lock_hold_count() -> u64 {
    SLOW_LOCK_HOLD_COUNT.load(Ordering::Relaxed)
}

/// Number of async lock acquisitions whose wait reached or exceeded their slow-wait threshold.
pub fn slow_lock_wait_count() -> u64 {
    SLOW_LOCK_WAIT_COUNT.load(Ordering::Relaxed)
}

/// Guard wrapping an async lock guard `G` to observe slow contention waits and long lock holds.
pub struct SlowLockGuard<G> {
    guard: G,
    name: &'static str,
    acquired_at: Instant,
    threshold: Duration,
}

impl<G> SlowLockGuard<G> {
    /// Acquire `lock_fut` under [`SLOW_ASYNC_LOCK_THRESHOLD`], warning on slow wait or hold.
    pub async fn acquire<F>(name: &'static str, lock_fut: F) -> Self
    where F: Future<Output = G> {
        Self::acquire_with_threshold(name, SLOW_ASYNC_LOCK_THRESHOLD, lock_fut).await
    }

    /// Acquire `lock_fut` under an explicit `threshold`, warning on slow wait or hold.
    pub async fn acquire_with_threshold<F>(
        name: &'static str,
        threshold: Duration,
        lock_fut: F,
    ) -> Self
    where
        F: Future<Output = G>,
    {
        let wait_started = Instant::now();
        let guard = lock_fut.await;
        let acquired_at = Instant::now();
        let waited = acquired_at.duration_since(wait_started);
        if waited >= threshold {
            SLOW_LOCK_WAIT_COUNT.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                lock = name,
                wait_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                threshold_ms = u64::try_from(threshold.as_millis()).unwrap_or(u64::MAX),
                "async lock acquisition waited behind a long-held lock"
            );
        }
        Self {
            guard,
            name,
            acquired_at,
            threshold,
        }
    }
}

impl<G> Deref for SlowLockGuard<G> {
    type Target = G;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<G> DerefMut for SlowLockGuard<G> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl<G> Drop for SlowLockGuard<G> {
    fn drop(&mut self) {
        let held = self.acquired_at.elapsed();
        if held >= self.threshold {
            SLOW_LOCK_HOLD_COUNT.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                lock = self.name,
                held_ms = u64::try_from(held.as_millis()).unwrap_or(u64::MAX),
                threshold_ms = u64::try_from(self.threshold.as_millis()).unwrap_or(u64::MAX),
                "async lock held across slow operation"
            );
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use futures::lock::Mutex as AsyncMutex;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn test_slow_lock_guard_records_long_hold_and_wait() {
        let mutex = std::sync::Arc::new(AsyncMutex::new(0_u32));
        let threshold = Duration::from_millis(50);
        let holds_before = slow_lock_hold_count();
        let waits_before = slow_lock_wait_count();

        let holder_mutex = std::sync::Arc::clone(&mutex);
        let holder = tokio::spawn(async move {
            let mut guard = SlowLockGuard::acquire_with_threshold(
                "test_slow_lock",
                threshold,
                holder_mutex.lock(),
            )
            .await;
            **guard += 1;
            tokio::time::sleep(Duration::from_millis(80)).await;
        });

        tokio::task::yield_now().await;
        let waiter_mutex = std::sync::Arc::clone(&mutex);
        let waiter = tokio::spawn(async move {
            let mut guard = SlowLockGuard::acquire_with_threshold(
                "test_slow_lock",
                threshold,
                waiter_mutex.lock(),
            )
            .await;
            **guard += 1;
        });

        holder.await.expect("holder completes");
        waiter.await.expect("waiter completes");

        assert!(slow_lock_hold_count() > holds_before);
        assert!(slow_lock_wait_count() > waits_before);
        assert_eq!(*mutex.lock().await, 2);
    }
}
