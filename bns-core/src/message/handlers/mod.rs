use crate::dht::{Did, PeerRing};
use crate::err::{Error, Result};
use crate::message::payload::{MessageRelay, MessageRelayMethod};
use crate::message::types::Message;
use crate::swarm::Swarm;

use futures::lock::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use web3::types::Address;

pub mod connection;
pub mod storage;

use connection::MessageConnection;

#[derive(Clone)]
pub struct MessageHandler {
    dht: Arc<Mutex<PeerRing>>,
    swarm: Arc<Swarm>,
}

impl MessageHandler {
    pub fn new(dht: Arc<Mutex<PeerRing>>, swarm: Arc<Swarm>) -> Self {
        Self { dht, swarm }
    }

    pub async fn send_message(
        &self,
        address: &Address,
        to_path: Option<VecDeque<Did>>,
        from_path: Option<VecDeque<Did>>,
        method: MessageRelayMethod,
        message: Message,
    ) -> Result<()> {
        // TODO: diff ttl for each message?
        let payload = MessageRelay::new(
            message,
            &self.swarm.session(),
            None,
            to_path,
            from_path,
            method,
        )?;
        self.swarm.send_message(address, payload).await
    }

    pub async fn handle_message_relay(&self, relay: &MessageRelay<Message>, prev: &Did) -> Result<()> {
        match &relay.data {
            Message::JoinDHT(ref msg) => self.join_chord(relay, prev, msg).await,
            Message::ConnectNode(ref msg) => self.connect_node(relay, prev, msg).await,
            Message::ConnectedNode(ref msg) => self.connected_node(relay, prev, msg).await,
            Message::AlreadyConnected(ref msg) => self.already_connected(relay, prev, msg).await,
            Message::FindSuccessor(ref msg) => self.find_successor(relay, prev, msg).await,
            Message::FoundSuccessor(ref msg) => self.found_successor(relay, prev, msg).await,
            Message::NotifyPredecessor(ref msg) => self.notify_predecessor(relay, prev, msg).await,
            Message::NotifiedPredecessor(ref msg) => {
                self.notified_predecessor(relay, prev, msg).await
            }
            x => Err(Error::MessageHandlerUnsupportMessageType(format!(
                "{:?}",
                x
            ))),
        }
    }

    /// This method is required because web-sys components is not `Send`
    /// which means a listening loop cannot running concurrency.
    pub async fn listen_once(&self) -> Option<MessageRelay<Message>> {
        if let Some(relay_message) = self.swarm.poll_message().await {
            if !relay_message.verify() {
                log::error!("Cannot verify msg or it's expired: {:?}", relay_message);
            }

            if let Err(e) = self
                .handle_message_relay(&relay_message, &relay_message.addr.into())
                .await
            {
                log::error!("Error in handle_message: {}", e);
            }
            Some(relay_message)
        } else {
            None
        }
    }
}

#[cfg(not(feature = "wasm"))]
mod listener {
    use super::MessageHandler;
    use crate::types::message::MessageListener;
    use async_trait::async_trait;
    use std::sync::Arc;

    use futures_util::pin_mut;
    use futures_util::stream::StreamExt;

    #[async_trait]
    impl MessageListener for MessageHandler {
        async fn listen(self: Arc<Self>) {
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
}

#[cfg(feature = "wasm")]
mod listener {
    use super::MessageHandler;
    use crate::poll;
    use crate::types::message::MessageListener;
    use async_trait::async_trait;
    use std::sync::Arc;
    use wasm_bindgen::UnwrapThrowExt;
    use wasm_bindgen_futures::spawn_local;

    #[async_trait(?Send)]
    impl MessageListener for MessageHandler {
        async fn listen(self: Arc<Self>) {
            let mut handler = Some(Arc::clone(&self));
            let mut func = move || {
                let handler = Arc::clone(&handler.take().unwrap_throw());
                spawn_local(Box::pin(async move {
                    handler.listen_once().await;
                }));
            };
            poll!(func, 200);
        }
    }
}
