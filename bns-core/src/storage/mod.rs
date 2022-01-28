mod memory;

use async_trait::async_trait;

pub use memory::MemStorage;

#[async_trait]
pub trait Storage {
    fn get(&self, addr: &str) -> Option<String>;
    fn set(&self, addr: &str, value: String) -> Option<String>;

    async fn fetch_dht(&self) {
        // fetch dht then use self.set to update
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memstorage_basic_interface_should_work() {
        let store = MemStorage::new();
        test_basic_interface(store);
    }

    #[test]
    fn memstorage_fetch_dht_should_work() {
        let store = MemStorage::new();
        test_fetch_dht(store);
    }

    fn test_basic_interface(store: impl Storage) {
        let v0 = store.set("42.chat.btc", "value 1".into());
        assert!(v0.is_none());

        let v1 = store.set("42.chat.btc", "value 2".into());
        assert_eq!(v1, Some("value 1".into()));

        let v2 = store.get("42.chat.btc");
        assert_eq!(v2, Some("value 2".into()));
    }

    fn test_fetch_dht(_store: impl Storage) {
        // Todo implement it
    }
}
