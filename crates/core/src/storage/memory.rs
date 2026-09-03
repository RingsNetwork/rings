//! In-memory key value storage.
//!
//! Budget law: a storage built with [`MemStorage::bounded`] never holds more than `capacity`
//! keys. When a `put` of a new key would exceed the bound, the least recently written keys are
//! retired first, one per excess key, so the bound restores before the new key is stored. A
//! `put` that rewrites an existing key never retires another key. A storage built with
//! [`MemStorage::new`] is unbounded.

use std::num::NonZeroU32;
use std::sync::RwLock;
use std::sync::RwLockReadGuard;
use std::sync::RwLockWriteGuard;

use async_trait::async_trait;

use super::write_ordered::WriteOrderedMap;
use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;

/// The table behind the lock together with its key budget.
#[derive(Debug)]
struct MemTable<V> {
    slots: WriteOrderedMap<V>,
    capacity: Option<NonZeroU32>,
}

impl<V> MemTable<V> {
    fn new(capacity: Option<NonZeroU32>) -> Self {
        Self {
            slots: WriteOrderedMap::default(),
            capacity,
        }
    }

    /// Whether storing `key` would add a key that the bound must make room for.
    fn bound_applies_to(&self, key: &str) -> Option<usize> {
        let capacity = self.capacity?;
        (!self.slots.contains(key)).then_some(capacity.get() as usize)
    }

    /// Store `value` under `key`, restoring the key budget first.
    ///
    /// Post: `slots.len() <= capacity` when a bound is set.
    fn put(&mut self, key: String, value: V) {
        if let Some(capacity) = self.bound_applies_to(&key) {
            while self.slots.len() >= capacity {
                if self.slots.pop_oldest().is_none() {
                    break;
                }
            }
        }
        self.slots.insert(key, value);
    }
}

/// In-memory storage implementation ordered by write recency.
#[derive(Debug)]
pub struct MemStorage<V> {
    table: RwLock<MemTable<V>>,
}

impl<V> Default for MemStorage<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> MemStorage<V> {
    /// Create an unbounded memory storage table.
    pub fn new() -> Self {
        Self {
            table: RwLock::new(MemTable::new(None)),
        }
    }

    /// Create a memory storage table holding at most `capacity` keys.
    pub fn bounded(capacity: NonZeroU32) -> Self {
        Self {
            table: RwLock::new(MemTable::new(Some(capacity))),
        }
    }

    fn read(&self) -> Result<RwLockReadGuard<'_, MemTable<V>>> {
        self.table.read().map_err(|_| Error::StorageLockPoisoned)
    }

    fn write(&self) -> Result<RwLockWriteGuard<'_, MemTable<V>>> {
        self.table.write().map_err(|_| Error::StorageLockPoisoned)
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl<V> KvStorageInterface<V> for MemStorage<V>
where V: Clone + Send + Sync
{
    async fn get(&self, key: &str) -> Result<Option<V>> {
        Ok(self.read()?.slots.get(key).cloned())
    }

    async fn put(&self, key: &str, value: &V) -> Result<()> {
        self.write()?.put(key.to_owned(), value.clone());
        Ok(())
    }

    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        Ok(self
            .read()?
            .slots
            .iter()
            .map(|(key, value)| (key.to_owned(), value.clone()))
            .collect())
    }

    async fn remove(&self, key: &str) -> Result<()> {
        self.write()?.slots.remove(key);
        Ok(())
    }

    async fn clear(&self) -> Result<()> {
        self.write()?.slots.clear();
        Ok(())
    }

    async fn count(&self) -> Result<u32> {
        let count = self.read()?.slots.len();
        u32::try_from(count).map_err(|_| Error::MessageSizeOverflow)
    }
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;

    fn capacity(keys: u32) -> NonZeroU32 {
        NonZeroU32::new(keys).expect("test capacity is non-zero")
    }

    #[tokio::test]
    async fn test_memstorage_basic_interface_should_work() {
        let store = MemStorage::new();
        let addr = SecretKey::random().address().to_string();

        assert_eq!(store.get(&addr).await.unwrap(), None);

        store.put(&addr, &"value 1".to_string()).await.unwrap();
        assert_eq!(store.get(&addr).await.unwrap(), Some("value 1".into()));

        store.put(&addr, &"value 2".to_string()).await.unwrap();
        assert_eq!(store.get(&addr).await.unwrap(), Some("value 2".into()));
    }

    /// Budget law: a new key beyond the bound retires the least recently written key.
    #[tokio::test]
    async fn test_bounded_put_retires_least_recently_written_key() -> Result<()> {
        let store = MemStorage::bounded(capacity(2));
        store.put("a", &1u8).await?;
        store.put("b", &2u8).await?;
        store.put("c", &3u8).await?;

        assert_eq!(store.count().await?, 2);
        assert_eq!(store.get("a").await?, None);
        assert_eq!(store.get("b").await?, Some(2));
        assert_eq!(store.get("c").await?, Some(3));
        Ok(())
    }

    /// Budget law: rewriting a stored key retires nothing and makes it the newest key.
    #[tokio::test]
    async fn test_bounded_rewrite_keeps_other_keys_and_refreshes_recency() -> Result<()> {
        let store = MemStorage::bounded(capacity(2));
        store.put("a", &1u8).await?;
        store.put("b", &2u8).await?;
        store.put("a", &3u8).await?;
        assert_eq!(store.count().await?, 2);
        assert_eq!(store.get("b").await?, Some(2));

        store.put("c", &4u8).await?;
        assert_eq!(store.get("b").await?, None);
        assert_eq!(store.get("a").await?, Some(3));
        assert_eq!(store.get("c").await?, Some(4));
        Ok(())
    }

    /// An unbounded storage never retires a key.
    #[tokio::test]
    async fn test_unbounded_storage_keeps_every_key() -> Result<()> {
        let store = MemStorage::new();
        for index in 0..64u8 {
            store.put(&index.to_string(), &index).await?;
        }
        assert_eq!(store.count().await?, 64);
        assert_eq!(store.get("0").await?, Some(0));
        Ok(())
    }
}
