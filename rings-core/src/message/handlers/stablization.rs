use async_trait::async_trait;

use crate::dht::ChordStablize;
use crate::dht::ChordStorage;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::err::Result;
use crate::message::types::Message;
use crate::message::types::NotifyPredecessorReport;
use crate::message::types::NotifyPredecessorSend;
use crate::message::types::SyncVNodeWithSuccessor;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::message::RelayMethod;
use crate::swarm::TransportManager;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<NotifyPredecessorSend> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &NotifyPredecessorSend,
    ) -> Result<()> {
        let mut dht = self.dht.lock().await;
        let mut relay = ctx.relay.clone();

        relay.relay(dht.id, None)?;
        dht.notify(msg.id);
        if let Some(id) = dht.predecessor {
            if id != relay.origin() {
                return self
                    .send_report_message(
                        Message::NotifyPredecessorReport(NotifyPredecessorReport { id }),
                        relay,
                    )
                    .await;
            }
        }
        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<NotifyPredecessorReport> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &NotifyPredecessorReport,
    ) -> Result<()> {
        let mut dht = self.dht.lock().await;
        let mut relay = ctx.relay.clone();

        relay.relay(dht.id, None)?;
        assert_eq!(relay.method, RelayMethod::REPORT);
        // if successor: predecessor is between (id, successor]
        // then update local successor
        if self.swarm.get_transport(&msg.id).is_none() && msg.id != self.swarm.address().into() {
            self.connect(&msg.id.into()).await?;
            return Ok(());
        }
        dht.successor.update(msg.id);
        if let Ok(PeerRingAction::RemoteAction(
            next,
            PeerRingRemoteAction::SyncVNodeWithSuccessor(data),
        )) = dht.sync_with_successor(msg.id)
        {
            self.send_direct_message(
                Message::SyncVNodeWithSuccessor(SyncVNodeWithSuccessor { data }),
                next,
            )
            .await?;
        }
        Ok(())
    }
}
