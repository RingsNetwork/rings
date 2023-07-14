//! This module including impls for Swarm
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;

use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::ConnectNodeReport;
use crate::message::ConnectNodeSend;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::prelude::RTCSdpType;
use crate::swarm::Swarm;
use crate::transports::manager::TransportHandshake;
use crate::transports::manager::TransportManager;
use crate::transports::Transport;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::ice_transport::IceTransportInterface;
use crate::types::ice_transport::IceTrickleScheme;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TransportManager for Swarm {
    type Transport = Arc<Transport>;

    async fn new_transport(&self) -> Result<Self::Transport> {
        let event_sender = self.transport_event_channel.sender();
        let mut ice_transport = Transport::new(event_sender);
        ice_transport
            .start(self.ice_servers.clone(), self.external_address.clone())
            .await?
            .apply_callback()
            .await?;

        Ok(Arc::new(ice_transport))
    }

    // register to swarm transports
    // should not wait connection statues here
    // a connection `Promise` may cause deadlock of both end
    async fn register(&self, did: Did, trans: Self::Transport) -> Result<()> {
        if trans.is_disconnected().await {
            return Err(Error::InvalidTransport);
        }

        tracing::info!("register transport {:?}", trans.id.clone());
        #[cfg(test)]
        {
            println!("register transport {:?}", trans.id.clone());
        }
        let id = trans.id;
        if let Some(t) = self.transports.get(&did) {
            if t.is_connected().await && !trans.is_connected().await {
                return Err(Error::InvalidTransport);
            }
            if t.id != id {
                self.transports.set(&did, trans);
                if let Err(e) = t.close().await {
                    tracing::error!("failed to close previous while registering {:?}", e);
                    return Err(Error::SwarmToClosePrevTransport(format!("{:?}", e)));
                }
                tracing::debug!("replace and closed previous connection! {:?}", t.id);
            }
        } else {
            self.transports.set(&did, trans);
        }
        Ok(())
    }

    async fn get_and_check_transport(&self, did: Did) -> Option<Self::Transport> {
        match self.get_transport(did) {
            Some(t) => {
                if t.is_disconnected().await {
                    tracing::debug!(
                        "[get_and_check_transport] transport {:?} is not connected will be drop",
                        t.id
                    );
                    if t.close().await.is_err() {
                        tracing::error!("Failed on close transport");
                    };
                    None
                } else {
                    Some(t)
                }
            }
            None => None,
        }
    }

    fn get_transport(&self, did: Did) -> Option<Self::Transport> {
        self.transports.get(&did)
    }

    fn remove_transport(&self, did: Did) -> Option<(Did, Self::Transport)> {
        self.transports.remove(&did)
    }

    fn get_dids(&self) -> Vec<Did> {
        self.transports.keys()
    }

    fn get_transports(&self) -> Vec<(Did, Self::Transport)> {
        self.transports.items()
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TransportHandshake for Swarm {
    type Transport = Arc<Transport>;
    type Payload = MessagePayload<Message>;

    async fn prepare_transport_offer(&self) -> Result<(Self::Transport, ConnectNodeSend)> {
        let trans = self.new_transport().await?;
        let offer = trans.get_handshake_info(RTCSdpType::Offer).await?;

        self.push_pending_transport(&trans)?;

        let offer_msg = ConnectNodeSend {
            transport_uuid: trans.id.to_string(),
            offer,
        };

        Ok((trans, offer_msg))
    }

    async fn answer_remote_transport(
        &self,
        did: Did,
        offer_msg: &ConnectNodeSend,
    ) -> Result<(Self::Transport, ConnectNodeReport)> {
        if self.get_and_check_transport(did).await.is_some() {
            return Err(Error::AlreadyConnected);
        };

        let trans = self.new_transport().await?;

        trans.register_remote_info(&offer_msg.offer, did).await?;
        let answer = trans.get_handshake_info(RTCSdpType::Answer).await?;

        self.push_pending_transport(&trans)?;

        let answer_msg = ConnectNodeReport {
            transport_uuid: offer_msg.transport_uuid.clone(),
            answer,
        };

        Ok((trans, answer_msg))
    }

    async fn create_offer(&self) -> Result<(Self::Transport, Self::Payload)> {
        let (transport, offer_msg) = self.prepare_transport_offer().await?;

        // This payload has fake destination and fake next_hop.
        // The invoker should fix it before sending if it is not a direct message.
        let payload = MessagePayload::new_send(
            Message::ConnectNodeSend(offer_msg),
            self.session_manager(),
            self.did(),
            self.did(),
        )?;

        Ok((transport, payload))
    }

    async fn answer_offer(
        &self,
        offer_payload: Self::Payload,
    ) -> Result<(Self::Transport, Self::Payload)> {
        tracing::info!("connect peer via offer: {:?}", offer_payload);

        if !offer_payload.verify() {
            return Err(Error::VerifySignatureFailed);
        }

        let (transport, answer_msg) = match &offer_payload.data {
            Message::ConnectNodeSend(ref msg) => {
                self.answer_remote_transport(offer_payload.relay.origin_sender(), msg)
                    .await
            }
            _ => Err(Error::InvalidMessage(
                "Should be ConnectNodeSend".to_string(),
            )),
        }?;

        // This payload has fake next_hop.
        // The invoker should fix it before sending if it is not a direct message.
        let answer_payload = MessagePayload::new_send(
            Message::ConnectNodeReport(answer_msg),
            self.session_manager(),
            self.did(),
            self.did(),
        )?;

        Ok((transport, answer_payload))
    }

    async fn accept_answer(&self, answer_payload: Self::Payload) -> Result<(Did, Self::Transport)> {
        tracing::debug!("accept_answer: {:?}", answer_payload);

        if !answer_payload.verify() {
            return Err(Error::VerifySignatureFailed);
        }

        match &answer_payload.data {
            Message::ConnectNodeReport(ref msg) => {
                let remote_did = answer_payload.relay.origin_sender();
                let transport_id = uuid::Uuid::from_str(&msg.transport_uuid)
                    .map_err(|_| Error::InvalidTransportUuid)?;

                let transport = self
                    .find_pending_transport(transport_id)?
                    .ok_or(Error::TransportNotFound)?;

                transport
                    .register_remote_info(&msg.answer, remote_did)
                    .await?;

                Ok((remote_did, transport))
            }

            _ => Err(Error::InvalidMessage(
                "Should be ConnectNodeReport".to_string(),
            )),
        }
    }
}
