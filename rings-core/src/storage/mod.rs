mod memory;
pub mod persistence;

pub use memory::MemStorage;
#[cfg(feature = "default")]
pub use persistence::redis::RedisStorage;
#[cfg(feature = "default")]
pub use persistence::redis::TPersistenceStorageOperation;
#[cfg(feature = "default")]
pub use persistence::redis::TPersistenceStorageReadAndWrite;
#[cfg(feature = "default")]
pub use persistence::redis::TPersistenceStorageRemove;

#[cfg(feature = "wasm")]
pub use self::persistence::idb::IDBStorage as Storage;
#[cfg(feature = "default")]
pub use self::persistence::kv::KvStorage as Storage;
pub use self::persistence::PersistenceStorageOperation;
pub use self::persistence::PersistenceStorageReadAndWrite;
pub use self::persistence::PersistenceStorageRemove;
