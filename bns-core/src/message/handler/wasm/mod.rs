mod helper;

use super::MessageHandler;
use crate::poll;
use crate::types::message::MessageListener;
use async_trait::async_trait;
use std::sync::Arc;
use wasm_bindgen_futures::spawn_local;

#[async_trait(?Send)]
impl MessageListener for MessageHandler {
    async fn listen(&'static self) {
        let msg_handler = Arc::new(self);
        let handler = Arc::clone(&msg_handler);
        let func = move || {
            let handler = Arc::clone(&handler);

            spawn_local(Box::pin(async move {
                handler.listen_once().await;
            }));
        };
        poll!(func, 200);
    }
}
