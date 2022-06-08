use dashmap::DashMap;
use std::hash::Hash;

#[derive(Clone, Debug, Default)]
pub struct MemStorage<K, V>
where
    K: Copy + Eq + Hash,
    V: Clone,
{
    table: DashMap<K, V>,
}

impl<K, V> MemStorage<K, V>
where
    K: Copy + Eq + Hash,
    V: Clone,
{
    pub fn new() -> Self {
        Self {
            table: DashMap::default(),
        }
    }

    pub fn get(&self, addr: &K) -> Option<V> {
        self.table.get(addr).map(|v| v.value().clone())
    }

    pub fn set(&self, addr: &K, value: V) -> Option<V> {
        self.table.insert(*addr, value)
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.len() == 0
    }

    pub fn get_or_set(&self, addr: &K, default: V) -> V {
        self.table.entry(*addr).or_insert(default).value().clone()
    }

    pub fn keys(&self) -> Vec<K> {
        self.table.clone().into_iter().map(|(k, _v)| k).collect()
    }

    pub fn values(&self) -> Vec<V> {
        self.table.clone().into_iter().map(|(_k, v)| v).collect()
    }

    pub fn items(&self) -> Vec<(K, V)> {
        self.table
            .clone()
            .into_iter()
            .map(|(k, v)| (k, v))
            .collect()
    }

    pub fn remove(&self, addr: &K) -> Option<(K, V)> {
        match self.get(addr) {
            Some(_) => self.table.remove(addr),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;
    use web3::types::Address;

    #[test]
    fn memstorage_basic_interface_should_work() {
        let store = MemStorage::<Address, String>::new();
        let addr = SecretKey::random().address();

        assert!(store.get(&addr).is_none());

        let v0 = store.set(&addr, "value 1".into());
        assert!(v0.is_none());

        let v1 = store.set(&addr, "value 2".into());
        assert_eq!(v1, Some("value 1".into()));

        let v2 = store.get(&addr);
        assert_eq!(v2, Some("value 2".into()));
    }
}
