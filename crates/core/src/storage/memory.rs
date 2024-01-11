use async_trait::async_trait;
use dashmap::DashMap;

use crate::error::Result;
use crate::storage::KvStorageInterface;

#[derive(Debug, Default)]
pub struct MemStorage<V>
where V: Clone
{
    table: DashMap<String, V>,
}

impl<V> MemStorage<V>
where V: Clone
{
    pub fn new() -> Self {
        Self {
            table: DashMap::default(),
        }
    }
}

#[async_trait]
impl<V> KvStorageInterface<V> for MemStorage<V>
where V: Clone + Send + Sync
{
    async fn get(&self, key: &str) -> Result<Option<V>> {
        Ok(self.table.get(&key.to_string()).map(|v| v.value().clone()))
    }

    async fn put(&self, key: &str, value: &V) -> Result<()> {
        self.table.insert(key.to_string(), value.clone());
        Ok(())
    }

    async fn get_all(&self) -> Result<Vec<(String, V)>> {
        Ok(self.table.clone().into_iter().collect())
    }

    async fn remove(&self, key: &str) -> Result<()> {
        match self.get(key).await? {
            Some(_) => self.table.remove(key),
            None => None,
        };
        Ok(())
    }

    async fn clear(&self) -> Result<()> {
        self.table.clear();
        Ok(())
    }

    async fn count(&self) -> Result<u32> {
        Ok(self.table.len() as u32)
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;

    #[tokio::test]
    async fn memstorage_basic_interface_should_work() {
        let store = MemStorage::new();
        let addr = SecretKey::random().address().to_string();

        assert_eq!(store.get(&addr).await.unwrap(), None);

        store.put(&addr, &"value 1".to_string()).await.unwrap();
        assert_eq!(store.get(&addr).await.unwrap(), Some("value 1".into()));

        store.put(&addr, &"value 2".to_string()).await.unwrap();
        assert_eq!(store.get(&addr).await.unwrap(), Some("value 2".into()));
    }
}
