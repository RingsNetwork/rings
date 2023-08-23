use async_trait::async_trait;
use rings_transport::core::callback::Callback;

use crate::channels::Channel;
use crate::error::Error;
use crate::error::Result;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::TransportEvent;

type TransportEventSender = <Channel<TransportEvent> as ChannelTrait<TransportEvent>>::Sender;

pub struct SwarmCallback {
    transport_event_sender: TransportEventSender,
}

impl SwarmCallback {
    pub fn new(transport_event_sender: TransportEventSender) -> Self {
        Self {
            transport_event_sender,
        }
    }
}

#[async_trait]
impl Callback for SwarmCallback {
    type Error = Error;

    async fn on_message(&self, _cid: &str, msg: &[u8]) -> Result<()> {
        Channel::send(
            &self.transport_event_sender,
            TransportEvent::DataChannelMessage(msg.into()),
        )
        .await
    }
}
