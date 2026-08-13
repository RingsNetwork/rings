use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use crate::error::Result;
use crate::sync_lock::lock;

/// Awaitable witness that blocks effects for the first observed key until explicitly released.
pub(crate) struct BlockingSendProbe<K> {
    blocked: AtomicBool,
    released: AtomicBool,
    blocked_key: Mutex<Option<K>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl<K> Default for BlockingSendProbe<K> {
    fn default() -> Self {
        Self {
            blocked: AtomicBool::new(false),
            released: AtomicBool::new(false),
            blocked_key: Mutex::new(None),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }
}

impl<K> BlockingSendProbe<K>
where K: Clone + Eq
{
    /// Block only the invocation that claims an empty probe.
    pub(crate) async fn block_first(&self, key: K) -> Result<()> {
        let claimed = self.register(key)?.0;
        if !claimed {
            return Ok(());
        }
        self.wait_for_release().await;
        Ok(())
    }

    /// Block every invocation carrying the first observed key.
    pub(crate) async fn block_key(&self, key: K) -> Result<()> {
        let (_, matches_claimed_key) = self.register(key)?;
        if !matches_claimed_key {
            return Ok(());
        }
        self.wait_for_release().await;
        Ok(())
    }

    fn register(&self, key: K) -> Result<(bool, bool)> {
        let mut blocked_key = lock(&self.blocked_key)?;
        let claimed = blocked_key.is_none();
        if claimed {
            *blocked_key = Some(key.clone());
            self.blocked.store(true, Ordering::Release);
            self.entered.notify_one();
        }
        Ok((claimed, blocked_key.as_ref() == Some(&key)))
    }

    async fn wait_for_release(&self) {
        while !self.released.load(Ordering::Acquire) {
            let released = self.release.notified();
            if self.released.load(Ordering::Acquire) {
                return;
            }
            released.await;
        }
    }

    pub(crate) async fn wait_until_blocked(&self) {
        while !self.blocked.load(Ordering::Acquire) {
            let entered = self.entered.notified();
            if self.blocked.load(Ordering::Acquire) {
                return;
            }
            entered.await;
        }
    }

    pub(crate) fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.release.notify_waiters();
    }
}
