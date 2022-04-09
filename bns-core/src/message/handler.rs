use crate::dht::{Chord, ChordAction, ChordRemoteAction, Did};
use crate::err::{Error, Result};
use crate::message::{
    AlreadyConnected, ConnectNode, ConnectedNode, FindSuccessor, FoundSuccessor, JoinDHT, Message,
    MessageRelay, MessageRelayMethod, MessageSessionRelayProtocol, NotifiedPredecessor,
    NotifyPredecessor,
};
use crate::swarm::{Swarm, TransportManager};
use crate::types::ice_transport::IceTrickleScheme;
use futures::lock::Mutex;
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
        match &relay.data {
            Message::JoinDHT(ref msg) => self.handle_join(relay, prev, msg).await,
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
            x => Err(Error::MessageHandlerUnsupportMessageType(format!(
                "{:?}",
                x
            ))),
        }
    }

    async fn handle_join(
        &self,
        relay: &MessageRelay<Message>,
        prev: &Did,
        msg: &JoinDHT,
    ) -> Result<()> {
        let mut dht = self.dht.lock().await;
        let relay = relay.clone();
        match dht.join(msg.id) {
            ChordAction::None => {
                log::debug!("Opps, {:?} is same as current", msg.id);
                Ok(())
            }
            ChordAction::RemoteAction(next, ChordRemoteAction::FindSuccessor(id)) => {
                if next != *prev {
                    self.send_message(
                        &next.into(),
                        Some(relay.to_path),
                        Some(relay.from_path),
                        MessageRelayMethod::SEND,
                        Message::FindSuccessor(FindSuccessor { id, for_fix: false }),
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            _ => unreachable!(),
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
            .ok_or(Error::MessageHandlerMissNextNode)?;
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
                    .register_remote_info(msg.handshake_info.to_owned().into())
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

                trans.wait_for_connected().await?;
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
                .ok_or(Error::MessageHandlerMissTransportAlreadyConnected),
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
                let transport = self
                    .swarm
                    .get_transport(&msg.answer_id)
                    .ok_or(Error::MessageHandlerMissTransportConnectedNode)?;
                transport
                    .register_remote_info(msg.handshake_info.clone().into())
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
        dht.notify(msg.id);
        self.send_message(
            &(*prev).into(),
            Some(relay.from_path),
            Some(relay.to_path),
            MessageRelayMethod::REPORT,
            Message::NotifiedPredecessor(NotifiedPredecessor {
                id: dht.id,
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
        dht.successor = msg.id;
        Ok(())
    }

    async fn handle_find_successor(
        &self,
        relay: &MessageRelay<Message>,
        prev: &Did,
        msg: &FindSuccessor,
    ) -> Result<()> {
        /*
         * A -> B For Example
         * B handle_find_successor then push_prev
         * now relay have paths follow:
         * {
         *     from_path: [A],
         *     to_path: []
         *     method: SEND
         * }
         * if found successor, then report back to A with new relay
         * which have paths follow:
         * {
         *     to_path: [A],
         *     from_path: [],
         *     method: REPORT
         * }
         * when A got report and handle_found_successor, after push_prev
         * that relay have paths follow:
         * {
         *     from_path: [B],
         *     to_path: []
         *     method: REPORT
         * }
         * because to_path.pop_back() assert_eq to current Did
         * then fix finger as request
         *
         * otherwise, B -> C
         * and then C get relay and push_prev, relay has paths follow:
         * {
         *     from_path: [A, B],
         *     to_path: [],
         *     method: SEND
         * }
         * if C found successor lucky, report to B, relay has paths follow:
         * {
         *     from_path: [],
         *     to_path: [A, B],
         *     method: REPORT
         * }
         * if B get message and handle_found_successor, after push_prev, relay has paths follow:
         * {
         *     from_path: [C],
         *     to_path: [A],
         *     method: REPORT
         * }
         * because to_path.pop_back() assert_eq to current Did
         * so B has been pop out of to_path
         *
         * if found to_path still have elements, recursivly report backward
         * now relay has path follow:
         * {
         *     to_path: [A],
         *     from_path: [C],
         *     method: REPORT
         * }
         * finally, relay handle_found_successor after push_prev, relay has paths follow:
         * {
         *     from_path: [C, B],
         *     to_path: []
         * }
         * because to_path.pop_back() assert_eq to current Did
         * A pop from to_path, and check to_path is empty
         * so update fix_finger_table with fix_finger_index
         */
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
                            id: id,
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
                dht.finger[fix_finger_index as usize] = Some(msg.id);
            } else {
                dht.successor = msg.id;
            }
            Ok(())
        }
    }

    /// This method is required because web-sys components is not `Send`
    /// which means a listening loop cannot running concurrency.
    pub async fn listen_once(&self) {
        if let Some(relay_message) = self.swarm.poll_message().await {
            if relay_message.is_expired() || !relay_message.verify() {
                log::error!("Cannot verify msg or it's expired: {:?}", relay_message);
            }

            if let Err(e) = self
                .handle_message_relay(&relay_message, &relay_message.addr.into())
                .await
            {
                log::error!("Error in handle_message: {}", e);
            }
        }
    }
}

#[cfg(not(feature = "wasm"))]
mod listener {
    use super::MessageHandler;
    use crate::types::message::MessageListener;
    use async_trait::async_trait;
    use std::sync::Arc;

    use futures_util::pin_mut;
    use futures_util::stream::StreamExt;

    #[async_trait]
    impl MessageListener for MessageHandler {
        async fn listen(self: Arc<Self>) {
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
}

#[cfg(feature = "wasm")]
mod listener {

    use super::MessageHandler;
    use crate::poll;
    use crate::types::message::MessageListener;
    use async_trait::async_trait;
    use std::sync::Arc;
    use wasm_bindgen::UnwrapThrowExt;
    use wasm_bindgen_futures::spawn_local;

    #[async_trait(?Send)]
    impl MessageListener for MessageHandler {
        async fn listen(self: Arc<Self>) {
            let mut handler = Some(Arc::clone(&self));
            let mut func = move || {
                let handler = Arc::clone(&handler.take().unwrap_throw());
                spawn_local(Box::pin(async move {
                    handler.listen_once().await;
                }));
            };
            poll!(func, 200);
        }
    }
}
