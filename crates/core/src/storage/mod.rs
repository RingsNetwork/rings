//! Traits about MemStorage and PersistenceStorage

mod memory;
pub mod persistence;

pub use memory::MemStorage;

#[cfg(feature = "wasm")]
pub use self::persistence::idb::IDBStorage as PersistenceStorage;
#[cfg(not(feature = "wasm"))]
pub use self::persistence::kv::KvStorage as PersistenceStorage;
pub use self::persistence::PersistenceStorageOperation;
pub use self::persistence::PersistenceStorageReadAndWrite;
pub use self::persistence::PersistenceStorageRemove;
