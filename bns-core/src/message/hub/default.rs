use crate::message::Message;
use crate::message::MessageRelay;
use crate::transports::default::DefaultTransport;
use crate::types::channel::Channel;
use crate::types::channel::Event;
use crate::types::ice_transport::IceTransport;
use crate::types::message::{MessageHandler, MessageHub, SendMessage, TransportManager};
use async_trait::async_trait;
use futures::StreamExt;

use crate::channels::default::AcChannel;
use anyhow::anyhow;
use anyhow::Result;

type MessagePayload = SendMessage<MessageRelay<Message>>;
type EventReceiver = <AcChannel<Event> as Channel<Event>>::Receiver;
type MessageReceiver = <AcChannel<MessagePayload> as Channel<MessagePayload>>::Receiver;

struct DefaultMessageHub<'a> {
    transport_manager:
        &'a dyn TransportManager<Event, AcChannel<Event>, Transport = DefaultTransport>,
    event_receiver: EventReceiver,
    message_receiver: MessageReceiver,
}

impl<'a> DefaultMessageHub<'a> {
    async fn send_message(&self, (address, message): &MessagePayload) -> Result<()> {
        match self.transport_manager.get_transport(address) {
            Some(trans) => trans.send_message(message).await,
            None => Err(anyhow!("cannot seek address in swarm table")),
        }
    }
}

#[async_trait]
impl<'a> MessageHub<'a, Event, MessageRelay<Message>, AcChannel<Event>, AcChannel<MessagePayload>>
    for DefaultMessageHub<'a>
{
    type Transport = DefaultTransport;

    fn new(
        transport_manager: &'a dyn TransportManager<
            Event,
            AcChannel<Event>,
            Transport = DefaultTransport,
        >,
        event_receiver: EventReceiver,
        message_receiver: MessageReceiver,
    ) -> Self {
        Self {
            transport_manager,
            event_receiver,
            message_receiver,
        }
    }

    async fn handle_message_io<T: Sync>(
        &self,
        message_handler: &dyn MessageHandler<
            T,
            Event,
            MessageRelay<Message>,
            AcChannel<Event>,
            AcChannel<MessagePayload>,
        >,
    ) {
        let mut recv_task = tokio::spawn(async move {
            while let Some(ev) = self.event_receiver.next().await {
                if let Ok(msg) = ev.try_into() {
                    message_handler.handle(msg).await;
                }
            }
        });

        let mut send_task = tokio::spawn(async move {
            while let Some(payload) = self.message_receiver.next().await {
                if self.send_message(&payload).await.is_err() {
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
