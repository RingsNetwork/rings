mod memory;

use async_trait::async_trait;

pub use memory::MemStorage;

#[async_trait]
pub trait Storage {
    type K;
    type V;

    fn new() -> Self;
    fn len(&self) -> usize;
    fn get(&self, addr: &Self::K) -> Option<Self::V>;
    fn set(&self, addr: &Self::K, value: Self::V) -> Option<Self::V>;
    fn get_or_set(&self, addr: &Self::K, default: Self::V) -> Self::V;
    fn keys(&self) -> Vec<Self::K>;
    fn values(&self) -> Vec<Self::V>;
    fn items(&self) -> Vec<(Self::K, Self::V)>;
    fn remove(&self, addr: &Self::K) -> Option<(Self::K, Self::V)>;
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
