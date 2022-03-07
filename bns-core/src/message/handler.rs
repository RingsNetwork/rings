use crate::dht::Chord;
use crate::message::*;
use crate::swarm::Swarm;
use anyhow::anyhow;
use anyhow::Result;
use futures_util::pin_mut;
use futures_util::stream::StreamExt;
use std::sync::Arc;
use web3::types::Address;

pub struct MessageHandler {
    dht: Arc<Chord>,
    swarm: Arc<Swarm>,
}

impl MessageHandler {
    pub fn new(dht: Arc<Chord>, swarm: Arc<Swarm>) -> Self {
        Self { dht, swarm }
    }

    pub async fn send_message(&self, address: &Address, message: Message) -> Result<()> {
        // TODO: diff ttl for each message?
        let payload = MessagePayload::new(message, &self.swarm.key, None)?;
        self.swarm.send_message(address, payload).await
    }

    pub async fn handle_message(&self, message: &Message, prev: &Did) -> Result<()> {
        let current = &self.dht.id;

        match message {
            Message::ConnectNode(msrp, msg) => {
                self.handle_connect_node(msrp.record(prev), msg).await
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

    async fn handle_connect_node(&self, _msrp: MsrpSend, _msg: &ConnectNode) -> Result<()> {
        // TODO: How to verify necessity based on Chord to decrease connections but make sure availablitity.
        Ok(())
    }

    async fn handle_already_connected(
        &self,
        _msrp: MsrpReport,
        _msg: &AlreadyConnected,
    ) -> Result<()> {
        Ok(())
    }

    async fn handle_connect_node_response(
        &self,
        _msrp: MsrpReport,
        _msg: &ConnectNodeResponse,
    ) -> Result<()> {
        Ok(())
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
