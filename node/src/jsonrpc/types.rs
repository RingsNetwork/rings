use crate::prelude::rings_rpc::response::CustomBackendMessage;

impl From<crate::backend::types::BackendMessage> for CustomBackendMessage {
    fn from(v: crate::backend::types::BackendMessage) -> Self {
        (v.message_type, base64::encode(v.data)).into()
    }
}
