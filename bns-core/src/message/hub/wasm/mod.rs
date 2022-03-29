use crate::message::Message;
use crate::message::MessageRelay;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::Event;
use crate::types::message::{MessageHandler, MessageHub, SendMessage, TransportManager};
use async_trait::async_trait;

use anyhow::anyhow;
use anyhow::Result;

#[cfg(not(feature = "wasm"))]
use crate::channels::default::AcChannel as Channel;
#[cfg(feature = "wasm")]
use crate::channels::wasm::CbChannel as Channel;

type EventReceiver = <Channel<Event> as ChannelTrait<Event>>::Receiver;
type MessageSender =
    <Channel<MessageRelay<Message>> as ChannelTrait<MessageRelay<Message>>>::Sender;

struct DefaultMessageHub {
    transport_manager: &dyn TransportManager,
    event_receiver: Channel<Event>,
    message_receiver: Channel<MessageRelay<Message>>,
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl MessageHub<Event, MessageRelay<Message>, Channel<Event>, Channel<MessageRelay<Message>>>
    for DefaultMessageHub
{
    fn new(
        transport_manager: &dyn TransportManager,
        event_receiver: EventReceiver,
        message_receiver: MessageSender,
    ) -> Self {
        Self {
            transport_manager,
            event_receiver,
            message_receiver,
        }
    }

    async fn poll_recv_message(&self) -> Option<MessageRelay<Message>> {
        let ev = Channel::recv(self.event_receiver).await.ok()??;
        ev.try_into().ok()
    }

    async fn poll_send_message(&self) -> Option<MessageRelay<Message>> {
        Channel::recv(self.message_receiver).await.ok()
    }

    async fn send_message(
        &self,
        (address, message): SendMessage<MessageRelay<Message>>,
    ) -> Result<()> {
        match self.transport_manager.get_transport(address) {
            Some(trans) => Ok(trans.send_message(message).await?),
            None => Err(anyhow!("cannot seek address in swarm table")),
        }
    }

    pub async fn handle_message_io(&self, message_handler: &dyn MessageHandler) {
        let mut recv_task = tokio::spawn(async move {
            while let Ok(Some(ev)) = Channel::recv(self.receiver).await {
                if let Ok(msg) = ev.try_into() {
                    self.message_handler.handle(msg).await;
                }
            }
        });

        let mut send_task = tokio::spawn(async move {
            while let Ok(Some(payload)) = Channel::recv(self.message_receiver).await {
                if self.send_message(payload).await.is_err() {
                    log::error!("Failed to send message");
                }
            }
        });

        tokio::select! {
            _v1 = &mut recv_task => send_task.abort(),
            _v2 = &mut send_task => recv_task.abort(),
        }
    }
}
