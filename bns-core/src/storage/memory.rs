use crate::storage::Storage;
use dashmap::DashMap;

#[derive(Clone, Debug, Default)]
pub struct MemStorage {
    table: DashMap<String, String>,
}

impl MemStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Storage for MemStorage {
    fn get(&self, addr: &str) -> Option<String> {
        self.table.get(addr).map(|v| v.value().clone())
    }

    fn set(&self, addr: &str, value: String) -> Option<String> {
        self.table.insert(addr.to_string(), value)
    }
}
