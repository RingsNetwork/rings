use super::payload_ng::RelayMessage;
use super::payload_ng::Message;
use crate::storage::MemStorage;
use std::any::type_name;
use std::sync::Arc;

struct MessageHandler<T> {
    handler: MemStorage<String, Arc<dyn FnMut(RelayMessage<T>) -> RelayMessage<T>>>
}
