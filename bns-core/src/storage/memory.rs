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

    fn get(&self, addr: &K) -> Option<V> {
        self.table.get(addr).map(|v| v.value().clone())
    }

    fn set(&self, addr: &K, value: V) -> Option<V> {
        self.table.insert(addr.clone(), value)
    }

    fn len(&self) -> usize {
        self.table.len()
    }

    fn is_empty(&self) -> bool {
        self.table.len() == 0
    }

    fn get_or_set(&self, addr: &Self::K, default: Self::V) -> Self::V {
        self.table
            .entry(addr.clone())
            .or_insert(default)
            .value()
            .clone()
    }

    fn keys(&self) -> Vec<Self::K> {
        self.table.clone().into_iter().map(|(k, _v)| k).collect()
    }

    fn values(&self) -> Vec<Self::V> {
        self.table.clone().into_iter().map(|(_k, v)| v).collect()
    }

    fn items(&self) -> Vec<(Self::K, Self::V)> {
        self.table
            .clone()
            .into_iter()
            .map(|(k, v)| (k, v))
            .collect()
    }

    fn remove(&self, addr: &Self::K) -> Option<(Self::K, Self::V)> {
        match self.get(addr) {
            Some(_) => self.table.remove(addr),
            None => None,
        }
    }
}
