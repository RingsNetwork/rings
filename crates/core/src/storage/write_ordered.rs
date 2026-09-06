//! A string-keyed map that remembers the order in which keys were last written.
//!
//! Every bounded storage backend needs the same two facts about its keys: the value under a
//! key, and which key was written least recently, so that a budget can be restored by retiring
//! that key first. This map keeps both under one invariant instead of letting each backend
//! maintain a parallel recency index.
//!
//! Invariant: `order` is the inverse of `written` restricted to `slots`, i.e.
//! `order[slots[k].written] = k` for every stored `k`, and `written` is injective because it is
//! drawn from a clock that strictly increases on every write. Hence the least recently written
//! key is the first key of `order`. The clock saturates at `u64::MAX`, so the invariant is
//! stated for fewer than `2^64` writes to one map, beyond any process lifetime.

use std::collections::BTreeMap;

/// One stored value with the write sequence that placed it.
#[derive(Debug)]
struct Slot<V> {
    written: u64,
    value: V,
}

/// A map ordered by write recency.
#[derive(Debug)]
pub(crate) struct WriteOrderedMap<V> {
    slots: BTreeMap<String, Slot<V>>,
    order: BTreeMap<u64, String>,
    clock: u64,
}

impl<V> Default for WriteOrderedMap<V> {
    fn default() -> Self {
        Self {
            slots: BTreeMap::new(),
            order: BTreeMap::new(),
            clock: 0,
        }
    }
}

impl<V> WriteOrderedMap<V> {
    /// The value stored under `key`.
    pub(crate) fn get(&self, key: &str) -> Option<&V> {
        self.slots.get(key).map(|slot| &slot.value)
    }

    /// Whether `key` is stored.
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.slots.contains_key(key)
    }

    /// Number of stored keys.
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// Store `value` under `key` as the most recently written key.
    ///
    /// Post: `key` is the last key of `order`; the previous value under `key`, if any, is
    /// returned.
    pub(crate) fn insert(&mut self, key: String, value: V) -> Option<V> {
        let previous = self.remove(&key);
        self.clock = self.clock.saturating_add(1);
        self.order.insert(self.clock, key.clone());
        self.slots.insert(key, Slot {
            written: self.clock,
            value,
        });
        previous
    }

    /// Remove `key`, returning its value.
    pub(crate) fn remove(&mut self, key: &str) -> Option<V> {
        let slot = self.slots.remove(key)?;
        self.order.remove(&slot.written);
        Some(slot.value)
    }

    /// The least recently written key, without removing it: what a backend that must succeed
    /// at an external effect before forgetting a key (the file store) retires next.
    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    pub(crate) fn oldest(&self) -> Option<&str> {
        self.order.first_key_value().map(|(_, key)| key.as_str())
    }

    /// Remove and return the least recently written key and its value.
    pub(crate) fn pop_oldest(&mut self) -> Option<(String, V)> {
        let (_, key) = self.order.pop_first()?;
        let slot = self.slots.remove(&key)?;
        Some((key, slot.value))
    }

    /// Every key and value, in key order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.slots
            .iter()
            .map(|(key, slot)| (key.as_str(), &slot.value))
    }

    /// Remove every key.
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.order.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Law: `pop_oldest` yields the key written least recently, and rewriting a key makes it
    /// newest.
    #[test]
    fn test_rewrite_moves_key_to_newest() {
        let mut map = WriteOrderedMap::default();
        map.insert("a".to_owned(), 1);
        map.insert("b".to_owned(), 2);

        assert_eq!(map.insert("a".to_owned(), 3), Some(1));
        assert_eq!(map.pop_oldest(), Some(("b".to_owned(), 2)));
        assert_eq!(map.pop_oldest(), Some(("a".to_owned(), 3)));
        assert_eq!(map.pop_oldest(), None);
    }

    /// Law: `oldest` names the key `pop_oldest` would remove, and removes nothing.
    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    #[test]
    fn test_oldest_peeks_without_removing() {
        let mut map = WriteOrderedMap::default();
        assert_eq!(map.oldest(), None);
        map.insert("a".to_owned(), 1);
        map.insert("b".to_owned(), 2);

        assert_eq!(map.oldest(), Some("a"));
        assert_eq!(map.len(), 2);
        assert_eq!(map.pop_oldest(), Some(("a".to_owned(), 1)));
        assert_eq!(map.oldest(), Some("b"));
    }

    /// Law: `order` and `slots` stay inverse under removal.
    #[test]
    fn test_remove_keeps_order_consistent() {
        let mut map = WriteOrderedMap::default();
        map.insert("a".to_owned(), 1);
        map.insert("b".to_owned(), 2);
        map.insert("c".to_owned(), 3);

        assert_eq!(map.remove("a"), Some(1));
        assert_eq!(map.remove("a"), None);
        assert_eq!(map.len(), 2);
        assert!(map.contains("c"));
        assert_eq!(map.iter().map(|(key, _)| key).collect::<Vec<_>>(), [
            "b", "c"
        ]);
    }
}
