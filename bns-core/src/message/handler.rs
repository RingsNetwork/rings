use crate::dht::{Chord, ChordAction, ChordRemoteAction, Did};
use crate::message::{
    AlreadyConnected, ConnectNode, ConnectedNode, FindSuccessor, FoundSuccessor, Message,
    MessageRelay, MessageRelayMethod, MessageSessionRelayProtocol, NotifiedPredecessor,
    NotifyPredecessor,
};
use crate::swarm::Swarm;
use crate::types::ice_transport::IceTrickleScheme;
use anyhow::anyhow;
use anyhow::Result;
use futures::lock::Mutex;
use futures_util::pin_mut;
use futures_util::stream::StreamExt;
use std::collections::VecDeque;
use std::sync::Arc;
use web3::types::Address;

#[cfg(feature = "wasm")]
use web_sys::RtcSdpType as RTCSdpType;
#[cfg(not(feature = "wasm"))]
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;

pub struct MessageHandler {
    dht: Arc<Mutex<Chord>>,
    swarm: Arc<Swarm>,
}

impl MessageHandler {
    pub fn new(dht: Arc<Mutex<Chord>>, swarm: Arc<Swarm>) -> Self {
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
        let payload =
            MessageRelay::new(message, &self.swarm.key, None, to_path, from_path, method)?;
        self.swarm.send_message(address, payload).await
    }

    pub async fn handle_message_relay(
        &self,
        relay: &MessageRelay<Message>,
        prev: &Did,
    ) -> Result<()> {
        match relay.data {
            Message::ConnectNode(ref msg) => self.handle_connect_node(relay, prev, msg).await,
            Message::ConnectedNode(ref msg) => self.handle_connected_node(relay, prev, msg).await,
            Message::AlreadyConnected(ref msg) => {
                self.handle_already_connected(relay, prev, msg).await
            }
            Message::FindSuccessor(ref msg) => self.handle_find_successor(relay, prev, msg).await,
            Message::FoundSuccessor(ref msg) => self.handle_found_successor(relay, prev, msg).await,
            Message::NotifyPredecessor(ref msg) => {
                self.handle_notify_predecessor(relay, prev, msg).await
            }
            Message::NotifiedPredecessor(ref msg) => {
                self.handle_notified_predecessor(relay, prev, msg).await
            }
            _ => Err(anyhow!("Unsupported message type")),
        }
    }

    async fn handle_connect_node(
        &self,
        relay: &MessageRelay<Message>,
        prev: &Did,
        msg: &ConnectNode,
    ) -> Result<()> {
        // TODO: Verify necessity based on Chord to decrease connections but make sure availablitity.

        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        let answer_id = dht.id;
        relay.push_prev(answer_id, *prev);
        if dht.id != msg.target_id {
            let next_node = match dht.find_successor(msg.target_id)? {
                ChordAction::Some(node) => Some(node),
                ChordAction::RemoteAction(node, _) => Some(node),
                _ => None,
            }
            .ok_or_else(|| anyhow!("Cannot find next node by dht"))?;
            return self
                .send_message(
                    &next_node,
                    Some(relay.to_path),
                    Some(relay.from_path),
                    MessageRelayMethod::SEND,
                    Message::ConnectNode(msg.clone()),
                )
                .await;
        }
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
                    &(*prev).into(),
                    Some(relay.from_path),
                    None,
                    MessageRelayMethod::REPORT,
                    Message::ConnectedNode(ConnectedNode {
                        answer_id,
                        handshake_info,
                    }),
                )
                .await?;

                trans.wait_for_connected(20, 3).await?;
                self.swarm.get_or_register(&msg.sender_id, trans).await?;

                Ok(())
            }

            _ => {
                self.send_message(
                    &(*prev).into(),
                    Some(relay.from_path),
                    None,
                    MessageRelayMethod::REPORT,
                    Message::AlreadyConnected(AlreadyConnected { answer_id }),
                )
                .await
            }
        }
    }

    async fn handle_already_connected(
        &self,
        relay: &MessageRelay<Message>,
        prev: &Did,
        msg: &AlreadyConnected,
    ) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, *prev);
        match relay.find_prev() {
            Some(prev_node) => {
                self.send_message(
                    &prev_node,
                    Some(relay.to_path),
                    Some(relay.from_path),
                    MessageRelayMethod::REPORT,
                    Message::AlreadyConnected(msg.clone()),
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

    async fn handle_connected_node(
        &self,
        relay: &MessageRelay<Message>,
        prev: &Did,
        msg: &ConnectedNode,
    ) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, *prev);
        match relay.find_prev() {
            Some(prev_node) => {
                self.send_message(
                    &prev_node,
                    Some(relay.to_path),
                    Some(relay.from_path),
                    MessageRelayMethod::REPORT,
                    Message::ConnectedNode(msg.clone()),
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

    async fn handle_notify_predecessor(
        &self,
        relay: &MessageRelay<Message>,
        prev: &Did,
        msg: &NotifyPredecessor,
    ) -> Result<()> {
        let mut dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, *prev);
        dht.notify(msg.predecessor);
        self.send_message(
            &(*prev).into(),
            Some(relay.from_path),
            Some(relay.to_path),
            MessageRelayMethod::REPORT,
            Message::NotifiedPredecessor(NotifiedPredecessor {
                predecessor: dht.predecessor.unwrap(),
            }),
        )
        .await
    }

    async fn handle_notified_predecessor(
        &self,
        relay: &MessageRelay<Message>,
        prev: &Did,
        msg: &NotifiedPredecessor,
    ) -> Result<()> {
        let mut dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, *prev);
        assert_eq!(relay.method, MessageRelayMethod::REPORT);
        // if successor: predecessor is between (id, successor]
        // then update local successor
        dht.successor = msg.predecessor;
        Ok(())
    }

    async fn handle_find_successor(
        &self,
        relay: &MessageRelay<Message>,
        prev: &Did,
        msg: &FindSuccessor,
    ) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, *prev);
        match dht.find_successor(msg.id) {
            Ok(action) => match action {
                ChordAction::Some(id) => {
                    self.send_message(
                        &(*prev).into(),
                        Some(relay.from_path),
                        Some(relay.to_path),
                        MessageRelayMethod::REPORT,
                        Message::FoundSuccessor(FoundSuccessor {
                            successor: id,
                            for_fix: msg.for_fix,
                        }),
                    )
                    .await
                }
                ChordAction::RemoteAction(next, ChordRemoteAction::FindSuccessor(id)) => {
                    self.send_message(
                        &next.into(),
                        Some(relay.to_path),
                        Some(relay.from_path),
                        MessageRelayMethod::SEND,
                        Message::FindSuccessor(FindSuccessor {
                            id,
                            for_fix: msg.for_fix,
                        }),
                    )
                    .await
                }
                _ => panic!(""),
            },
            Err(e) => panic!("{:?}", e),
        }
    }

    async fn handle_found_successor(
        &self,
        relay: &MessageRelay<Message>,
        prev: &Did,
        msg: &FoundSuccessor,
    ) -> Result<()> {
        let mut dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, *prev);
        if !relay.to_path.is_empty() {
            self.send_message(
                &(*prev).into(),
                Some(relay.to_path),
                Some(relay.from_path),
                MessageRelayMethod::REPORT,
                Message::FoundSuccessor(msg.clone()),
            )
            .await
        } else {
            if msg.for_fix {
                let fix_finger_index = dht.fix_finger_index;
                dht.finger[fix_finger_index as usize] = Some(msg.successor);
            } else {
                dht.successor = msg.successor;
            }
            Ok(())
        }
    }

    pub async fn listen(&self) {
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
