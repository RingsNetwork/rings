use super::MessageHandler;
use crate::types::message::MessageListener;
use async_trait::async_trait;

use futures_util::pin_mut;
use futures_util::stream::StreamExt;

#[async_trait]
impl MessageListener for MessageHandler {
    async fn listen(&'static self) {
        let relay_messages = self.swarm.iter_messages();

        pin_mut!(relay_messages);

        while let Some(relay_message) = relay_messages.next().await {
            if relay_message.is_expired() || !relay_message.verify() {
                log::error!("Cannot verify msg or it's expired: {:?}", relay_message);
                continue;
            }

            if let Err(e) = self
                .handle_message_relay(&relay_message, &relay_message.addr.into())
                .await
            {
                log::error!("Error in handle_message: {}", e);
                continue;
            }
        }
    }
}
