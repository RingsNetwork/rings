mod memory;

use async_trait::async_trait;

pub use memory::MemStorage;

#[async_trait]
pub trait PersistentStorage {}
