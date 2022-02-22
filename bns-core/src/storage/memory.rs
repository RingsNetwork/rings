use crate::storage::Storage;
use dashmap::DashMap;
use std::hash::Hash;

#[derive(Clone, Debug)]
pub struct MemStorage<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    table: DashMap<K, V>,
}

impl<K, V> Storage for MemStorage<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    type K = K;
    type V = V;

    fn new() -> Self {
        Self {
            table: DashMap::new(),
        }
    }

    fn get(&self, addr: K) -> Option<V> {
        self.table.get(&addr).map(|v| v.value().clone())
    }

    fn set(&self, addr: K, value: V) -> Option<V> {
        self.table.insert(addr, value)
    }
}
