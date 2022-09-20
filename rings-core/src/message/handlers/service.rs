use async_trait::async_trait;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::err::Error;
use crate::err::Result;
use crate::message::types::FindSuccessorSend;
use crate::message::types::RequestServiceReport;
use crate::message::FindSuccessorAnd;
use crate::message::FindSuccessorThen;
use crate::message::HandleMsg;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TChordSocketForward {
    async fn request(&self, id: Did, req: Vec<u8>) -> Result<()>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TChordSocketForward for MessageHandler {
    async fn request(&self, id: Did, req: Vec<u8>) -> Result<()> {
        let connect_msg = Message::FindSuccessorSend(FindSuccessorSend {
            id,
            strict: true,
            report_then: FindSuccessorThen::None,
            and: FindSuccessorAnd::RequestService(req),
        });
        let next_hop = {
            match self.dht.find_successor(id)? {
                PeerRingAction::Some(node) => Some(node),
                PeerRingAction::RemoteAction(node, _) => Some(node),
                _ => None,
            }
        }
        .ok_or(Error::NoNextHop)?;
        log::debug!("next_hop: {:?}", next_hop);
        self.send_message(connect_msg, next_hop, id).await
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl HandleMsg<RequestServiceReport> for MessageHandler {
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &RequestServiceReport,
    ) -> Result<()> {
        let mut relay = ctx.relay.clone();
        relay.relay(self.dht.id, None)?;
        if relay.next_hop.is_some() {
            self.transpond_payload(ctx, relay).await
        } else {
            log::info!("got service content {:?}", &msg.data);
            Ok(())
        }
    }
}
