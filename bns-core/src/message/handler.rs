use crate::dht::{Chord, ChordAction, Did, RemoteAction};
use crate::message::{
    AlreadyConnected, ConnectNode, ConnectedNode, FindSuccessor, FoundSuccessor, Message,
    MessageRelay, MessageRelayMethod, NotifiedPredecessor, NotifyPredecessor,
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
    swarm: Arc<Mutex<Swarm>>,
}

impl MessageHandler {
    pub fn new(dht: Arc<Mutex<Chord>>, swarm: Arc<Mutex<Swarm>>) -> Self {
        Self { dht, swarm }
    }

    pub async fn send_message(
        &self,
        address: &Address,
        message: Message,
        method: MessageRelayMethod,
    ) -> Result<()> {
        // TODO: diff ttl for each message?
        let swarm = self.swarm.lock().await;
        let payload = MessageRelay::new(message, &swarm.key, None, method)?;
        swarm.send_message(address, payload).await
    }

    pub async fn handle_message_relay(&self, relay: &MessageRelay, prev: &Did) -> Result<()> {
        match message {
            Message::ConnectNode(msg) => self.handle_connect_node(relay, prev, msg).await,
            Message::ConnectedNode(msg) => self.handle_connected_node(prev, msg).await,
            Message::AlreadyConnected(msg) => self.handle_already_connected(relay, prev, msg).await,
            Message::FindSuccessor(relay, msg) => {
                self.handle_find_successor(relay, prev, msg).await
            }
            Message::FoundSuccessor(relay, msg) => {
                self.handle_found_successor(relay, prev, msg).await
            }
            Message::NotifyPredecessor(relay, msg) => {
                self.handle_notify_predecessor(relay, prev, msg).await
            }
            Message::NotifiedPredecessor(relay, msg) => {
                self.handle_notified_predecessor(relay, prev, msg).await
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
        _relay: &MessageRelay,
        _prev: &Did,
        _msg: &AlreadyConnected,
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

    async fn handle_notify_predecessor(
        &self,
        relay: &MessageRelay,
        prev: &Did,
        msg: &NotifyPredecessor,
    ) -> Result<()> {
        let mut chord = self.dht.lock().await;
        chord.notify(msg.predecessor);
        let mut relay = relay.clone();
        relay.push_prev(prev);
        let report =
            MessageRelay::get_report_relay(&relay, relay.tx_id.clone(), relay.message_id.clone());
        let message = Message::NotifiedPredecessor(
            report,
            NotifiedPredecessor {
                predecessor: chord.predecessor.unwrap(),
            },
        );
        self.send_message(&(prev.clone().into()), message).await
    }

    async fn handle_notified_predecessor(
        &self,
        relay: &MessageRelay,
        _prev: &Did,
        msg: &NotifiedPredecessor,
    ) -> Result<()> {
        let mut chord = self.dht.lock().await;
        assert_eq!(relay.protocol, RelayProtocol::REPORT);
        // if successor: predecessor is between (id, successor]
        // then update local successor
        chord.successor = msg.predecessor;
        Ok(())
    }

    async fn handle_find_successor(
        &self,
        relay: &MessageRelay,
        prev: &Did,
        msg: &FindSuccessor,
    ) -> Result<()> {
        let chord = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(prev);
        match chord.find_successor(msg.id) {
            Ok(action) => match action {
                ChordAction::Some(id) => {
                    let report = MessageRelay::get_report_relay(
                        &relay,
                        relay.tx_id.clone(),
                        relay.message_id.clone(),
                    );
                    let message = Message::FoundSuccessor(
                        report,
                        FoundSuccessor {
                            successor: id,
                            for_fix: msg.for_fix,
                        },
                    );
                    self.send_message(&prev.clone().into(), message).await
                }
                ChordAction::RemoteAction((next, RemoteAction::FindSuccessor(id))) => {
                    let send = MessageRelay::get_send_relay(
                        &relay,
                        relay.tx_id.clone(),
                        relay.message_id.clone(),
                        chord.id,
                        next,
                    );
                    let message = Message::FindSuccessor(
                        send,
                        FindSuccessor {
                            id,
                            for_fix: msg.for_fix,
                        },
                    );
                    self.send_message(&next.into(), message).await
                }
                _ => panic!(""),
            },
            Err(e) => panic!("{:?}", e),
        }
    }

    async fn handle_found_successor(
        &self,
        relay: &MessageRelay,
        prev: &Did,
        msg: &FoundSuccessor,
    ) -> Result<()> {
        let mut chord = self.dht.lock().await;
        let current = chord.id;
        assert_eq!(relay.protocol, RelayProtocol::REPORT);
        let mut relay = relay.clone();
        assert_eq!(Some(current), relay.remove_to_path());
        relay.remove_from_path();
        if !relay.to_path.is_empty() {
            let prev = relay.find_prev();
            let report = MessageRelay::get_report_relay(
                &relay,
                relay.tx_id.clone(),
                relay.message_id.clone(),
            );
            let message = Message::FoundSuccessor(report, msg.clone());
            self.send_message(&prev.unwrap().into(), message).await
        } else {
            if msg.for_fix {
                let fix_finger_index = chord.fix_finger_index;
                chord.finger[fix_finger_index as usize] = Some(msg.successor);
            } else {
                chord.successor = msg.successor;
            }
            Ok(())
        }
    }

    pub async fn listen(&self) {
        let swarm = self.swarm.lock().await;
        let payloads = swarm.iter_messages();

        pin_mut!(payloads);

        while let Some(payload) = payloads.next().await {
            if payload.is_expired() || !payload.verify() {
                log::error!("Cannot verify msg or it's expired: {:?}", payload);
                continue;
            }

            if let Err(e) = self.handle_payload(&payload, &payload.addr.into()).await {
                log::error!("Error in handle_message: {}", e);
                continue;
            }
        }
    }

    pub async fn stabilize(&self) {
        loop {
            let mut chord = self.dht.lock().await;
            let (current, successor) = (chord.id, chord.successor);
            let tx_id = String::from("");
            let message_id = String::from("");
            let mut to_path = VecDeque::new();
            let mut from_path = VecDeque::new();
            to_path.push_back(successor);
            from_path.push_back(current);
            let relay = MessageRelay::new_with_path(
                tx_id,
                message_id,
                to_path,
                from_path,
                RelayProtocol::SEND,
            );
            let message = Message::NotifyPredecessor(
                relay,
                NotifyPredecessor {
                    predecessor: current,
                },
            );
            self.send_message(&current.into(), message).await.unwrap();

            // fix fingers
            match chord.fix_fingers() {
                Ok(action) => match action {
                    ChordAction::None => {
                        log::info!("")
                    }
                    ChordAction::RemoteAction((
                        next,
                        RemoteAction::FindSuccessorForFix(current),
                    )) => {
                        let tx_id = String::from("");
                        let message_id = String::from("");
                        let relay = MessageRelay::new_with_node(
                            tx_id,
                            message_id,
                            next,
                            current,
                            RelayProtocol::SEND,
                        );
                        let message = Message::FindSuccessor(
                            relay,
                            FindSuccessor {
                                id: current,
                                for_fix: true,
                            },
                        );
                        self.send_message(&next.into(), message).await.unwrap();
                    }
                    _ => {
                        log::error!("Invalid Chord Action");
                    }
                },
                Err(e) => log::error!("{:?}", e),
            }
        }
    }
}
