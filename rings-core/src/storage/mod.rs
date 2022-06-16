mod memory;
pub mod persistence;

pub use memory::MemStorage;

#[cfg(feature = "wasm")]
pub use self::persistence::idb::IDBStorage as Storage;
#[cfg(not(feature = "wasm"))]
pub use self::persistence::kv::KvStorage as Storage;
pub use self::persistence::PersistenceStorageOperation;
pub use self::persistence::PersistenceStorageReadAndWrite;
pub use self::persistence::PersistenceStorageRemove;
