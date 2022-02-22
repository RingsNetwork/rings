mod memory;

use async_trait::async_trait;

pub use memory::MemStorage;

#[async_trait]
pub trait Storage {
    type K;
    type V;

    fn new() -> Self;
    fn get(&self, addr: Self::K) -> Option<Self::V>;
    fn set(&self, addr: Self::K, value: Self::V) -> Option<Self::V>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memstorage_basic_interface_should_work() {
        let store = MemStorage::<&str, String>::new();

        assert!(store.get("42.chat.btc").is_none());

        let v0 = store.set("42.chat.btc", "value 1".into());
        assert!(v0.is_none());

        let v1 = store.set("42.chat.btc", "value 2".into());
        assert_eq!(v1, Some("value 1".into()));

        let v2 = store.get("42.chat.btc");
        assert_eq!(v2, Some("value 2".into()));
    }
}
