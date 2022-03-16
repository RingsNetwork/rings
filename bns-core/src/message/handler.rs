use crate::dht::{Chord, ChordAction};
use crate::message::*;
use crate::swarm::Swarm;
use crate::types::ice_transport::IceTrickleScheme;
use anyhow::anyhow;
use anyhow::Result;
use futures_util::pin_mut;
use futures_util::stream::StreamExt;
use std::sync::Arc;
use web3::types::Address;

#[cfg(feature = "wasm")]
use web_sys::RtcSdpType as RTCSdpType;
#[cfg(not(feature = "wasm"))]
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;

pub struct MessageHandler {
    dht: Arc<Chord>,
    swarm: Arc<Swarm>,
}

impl MessageHandler {
    pub fn new(dht: Arc<Chord>, swarm: Arc<Swarm>) -> Self {
        Self { dht, swarm }
    }

    pub async fn send_message(&self, address: &Address, message: Message) -> Result<()> {
        // TODO: Diff ttl for each message?
        let payload = MessagePayload::new(message, &self.swarm.key, None)?;
        self.swarm.send_message(address, payload).await
    }

    pub async fn handle_message(&self, message: &Message, prev: &Did) -> Result<()> {
        let current = &self.dht.id;

        match message {
            Message::ConnectNode(msrp, msg) => {
                self.handle_connect_node(&msrp.record(prev), msg).await
            }
            Message::AlreadyConnected(msrp, msg) => {
                self.handle_already_connected(msrp.record(prev, current)?, msg)
                    .await
            }
            Message::ConnectNodeResponse(msrp, msg) => {
                self.handle_connect_node_response(msrp.record(prev, current)?, msg)
                    .await
            }
            _ => Err(anyhow!("Unsupported message type")),
        }
    }

    async fn handle_connect_node(&self, msrp: &MsrpSend, msg: &ConnectNode) -> Result<()> {
        // TODO: Verify necessity based on Chord to decrease connections but make sure availablitity.

        if self.dht.id != msg.target_id {
            let next_node = match self.dht.find_successor(msg.target_id)? {
                ChordAction::Some(node) => Some(node),
                ChordAction::RemoteAction(node, _) => Some(node),
                _ => None,
            }
            .ok_or_else(|| anyhow!("Cannot find next node by dht"))?;

            return self
                .send_message(&next_node, Message::ConnectNode(msrp.clone(), msg.clone()))
                .await;
        }

        let prev_node = msrp
            .from_path
            .last()
            .ok_or_else(|| anyhow!("Cannot get node"))?;
        let answer_id = self.dht.id;

        match self.swarm.get_transport(&msg.sender_id) {
            None => {
                let trans = self.swarm.new_transport().await?;
                trans
                    .register_remote_info(msg.handshake_info.clone().try_into()?)
                    .await?;

                let handshake_info = trans
                    .get_handshake_info(self.swarm.key, RTCSdpType::Answer)
                    .await?
                    .to_string();

                self.send_message(
                    prev_node,
                    Message::ConnectNodeResponse(
                        msrp.into(),
                        ConnectNodeResponse {
                            answer_id,
                            handshake_info,
                        },
                    ),
                )
                .await?;

                trans.wait_for_connected(20, 3).await?;
                self.swarm.get_or_register(&msg.sender_id, trans);

                Ok(())
            }

            _ => {
                self.send_message(
                    prev_node,
                    Message::AlreadyConnected(msrp.into(), AlreadyConnected { answer_id }),
                )
                .await
            }
        }
    }

    async fn handle_already_connected(
        &self,
        msrp: MsrpReport,
        msg: &AlreadyConnected,
    ) -> Result<()> {
        match msrp.to_path.last() {
            Some(prev_node) => {
                self.send_message(
                    prev_node,
                    Message::AlreadyConnected(msrp.clone(), msg.clone()),
                )
                .await
            }
            None => self
                .swarm
                .get_transport(&msg.answer_id)
                .map(|_| ())
                .ok_or_else(|| anyhow!("Receive AlreadyConnected but cannot get transport")),
        }
    }

    async fn handle_connect_node_response(
        &self,
        msrp: MsrpReport,
        msg: &ConnectNodeResponse,
    ) -> Result<()> {
        match msrp.to_path.last() {
            Some(prev_node) => {
                self.send_message(
                    prev_node,
                    Message::ConnectNodeResponse(msrp.clone(), msg.clone()),
                )
                .await
            }
            None => {
                let trans = self.swarm.get_transport(&msg.answer_id).ok_or_else(|| {
                    anyhow!("Cannot get trans while handle connect node response")
                })?;

                trans
                    .register_remote_info(msg.handshake_info.clone().try_into()?)
                    .await
                    .map(|_| ())
            }
        }
    }

    pub async fn listen(&self) {
        let payloads = self.swarm.clone().iter_messages();

        pin_mut!(payloads);

        while let Some(payload) = payloads.next().await {
            if payload.is_expired() || !payload.verify() {
                log::error!("Cannot verify msg or it's expired: {:?}", payload);
                continue;
            }

            if let Err(e) = self
                .handle_message(&payload.data, &payload.addr.into())
                .await
            {
                log::error!("Error in handle_message: {}", e);
                continue;
            }
        }
    }
}
