mod memory;
pub mod persistence;

use async_trait::async_trait;

pub use memory::MemStorage;

#[cfg(not(features = "wasm"))]
pub use persistence::redis::{
    RedisStorage, TPersistenceStorageOperation, TPersistenceStorageReadAndWrite,
    TPersistenceStorageRemove,
};

#[cfg(feature = "wasm")]
pub use self::persistence::idb::IDBStorage as Storage;
#[cfg(feature = "default")]
pub use self::persistence::kv::KvStorage as Storage;
pub use self::persistence::PersistenceStorageOperation;
pub use self::persistence::PersistenceStorageReadAndWrite;
pub use self::persistence::PersistenceStorageRemove;
