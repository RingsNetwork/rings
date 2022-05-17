use crate::dht::{Chord, ChordStablize, Did, PeerRingAction, PeerRingRemoteAction};
use crate::err::{Error, Result};
use crate::message::payload::{MessageRelay, MessageRelayMethod};
use crate::message::protocol::MessageSessionRelayProtocol;
use crate::message::types::{
    AlreadyConnected, ConnectNodeReport, ConnectNodeSend, FindSuccessorReport, FindSuccessorSend,
    JoinDHT, Message, NotifyPredecessorReport, NotifyPredecessorSend,
};
use crate::message::MessageHandler;
use crate::swarm::TransportManager;
use crate::types::ice_transport::IceTrickleScheme;

use crate::prelude::RTCSdpType;
use async_trait::async_trait;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TChordConnection {
    async fn join_chord(&self, relay: MessageRelay<Message>, prev: Did, msg: JoinDHT)
        -> Result<()>;

    async fn connect_node(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: ConnectNodeSend,
    ) -> Result<()>;

    async fn connected_node(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: ConnectNodeReport,
    ) -> Result<()>;

    async fn already_connected(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: AlreadyConnected,
    ) -> Result<()>;

    async fn find_successor(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: FindSuccessorSend,
    ) -> Result<()>;

    async fn found_successor(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: FindSuccessorReport,
    ) -> Result<()>;

    async fn notify_predecessor(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: NotifyPredecessorSend,
    ) -> Result<()>;

    async fn notified_predecessor(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: NotifyPredecessorReport,
    ) -> Result<()>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TChordConnection for MessageHandler {
    async fn join_chord(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: JoinDHT,
    ) -> Result<()> {
        // here is two situation.
        // finger table just have no other node(beside next), it will be a `create` op
        // otherwise, it will be a `send` op
        let mut dht = self.dht.lock().await;
        let mut relay = relay.clone();
        match dht.join(msg.id) {
            PeerRingAction::None => Ok(()),
            PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessor(id)) => {
                // if there is only two nodes A, B, it may cause recursion
                // A.successor == B
                // B.successor == A
                // A.find_successor(B)
                if next != prev {
                    relay.relay(Some(next));
                    self.send_message(
                        &next.into(),
                        Some(relay.to_path),
                        Some(relay.from_path),
                        MessageRelayMethod::SEND,
                        Message::FindSuccessorSend(FindSuccessorSend { id, for_fix: false }),
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            _ => unreachable!(),
        }
    }

    async fn connect_node(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: ConnectNodeSend,
    ) -> Result<()> {
        // TODO: Verify necessity based on PeerRing to decrease connections but make sure availablitity.
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, prev);
        if dht.id != msg.target_id {
            let next_node = match dht.find_successor(msg.target_id)? {
                PeerRingAction::Some(node) => Some(node),
                PeerRingAction::RemoteAction(node, _) => Some(node),
                _ => None,
            }
            .ok_or(Error::MessageHandlerMissNextNode)?;
            return self
                .send_message(
                    &next_node,
                    Some(relay.to_path),
                    Some(relay.from_path),
                    MessageRelayMethod::SEND,
                    Message::ConnectNodeSend(msg.clone()),
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
                    .get_handshake_info(self.swarm.session(), RTCSdpType::Answer)
                    .await?
                    .to_string();
                self.send_message(
                    &prev.into(),
                    Some(relay.from_path),
                    None,
                    MessageRelayMethod::REPORT,
                    Message::ConnectNodeReport(ConnectNodeReport {
                        answer_id: dht.id,
                        handshake_info,
                    }),
                )
                .await?;
                self.swarm.get_or_register(&msg.sender_id, trans).await?;

                Ok(())
            }

            _ => {
                self.send_message(
                    &prev.into(),
                    Some(relay.from_path),
                    None,
                    MessageRelayMethod::REPORT,
                    Message::AlreadyConnected(AlreadyConnected { answer_id: dht.id }),
                )
                .await
            }
        }
    }

    async fn connected_node(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: ConnectNodeReport,
    ) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, prev);
        if let Some(prev_node) = relay.next() {
            self.send_message(
                &prev_node,
                Some(relay.to_path),
                Some(relay.from_path),
                MessageRelayMethod::REPORT,
                Message::ConnectNodeReport(msg.clone()),
            )
                .await
        } else {
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

    async fn already_connected(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: AlreadyConnected,
    ) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, prev);
        if let Some(prev_node) = relay.next() {
            self.send_message(
                &prev_node,
                Some(relay.to_path),
                Some(relay.from_path),
                MessageRelayMethod::REPORT,
                Message::AlreadyConnected(msg.clone()),
            )
                .await
        } else {
            self
                .swarm
                .get_transport(&msg.answer_id)
                .map(|_| ())
                .ok_or(Error::MessageHandlerMissTransportAlreadyConnected)
        }
    }

    async fn find_successor(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: FindSuccessorSend,
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
        match dht.find_successor(msg.id)? {
            PeerRingAction::Some(id) => {
                self.send_message(
                    &prev.into(),
                    Some(relay.to_path),
                    Some(relay.from_path),
                    MessageRelayMethod::REPORT,
                    Message::FindSuccessorReport(FindSuccessorReport {
                        id,
                        for_fix: msg.for_fix,
                    }),
                )
                .await
            }
            PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessor(id)) => {
                relay.relay(Some(next));
                self.send_message(
                    &next.into(),
                    Some(relay.to_path),
                    Some(relay.from_path),
                    MessageRelayMethod::SEND,
                    Message::FindSuccessorSend(FindSuccessorSend {
                        id,
                        for_fix: msg.for_fix,
                    }),
                )
                .await
            }
            act => Err(Error::PeerRingUnexpectedAction(act)),
        }
    }

    async fn found_successor(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: FindSuccessorReport,
    ) -> Result<()> {
        let mut dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.relay(None);
        if let Some(next) = relay.next() {
            self.send_message(
                &next.into(),
                Some(relay.to_path),
                Some(relay.from_path),
                MessageRelayMethod::REPORT,
                Message::FindSuccessorReport(msg.clone()),
            )
            .await
        } else {
            if msg.for_fix {
                let fix_finger_index = dht.fix_finger_index;
                dht.finger[fix_finger_index as usize] = Some(msg.id);
            } else {
                dht.successor.update(msg.id);
            }
            Ok(())
        }
    }

    async fn notify_predecessor(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: NotifyPredecessorSend,
    ) -> Result<()> {
        let mut dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, prev);
        dht.notify(msg.id);
        self.send_message(
            &prev.into(),
            Some(relay.from_path),
            Some(relay.to_path),
            MessageRelayMethod::REPORT,
            Message::NotifyPredecessorReport(NotifyPredecessorReport { id: dht.id }),
        )
        .await
    }

    async fn notified_predecessor(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: NotifyPredecessorReport,
    ) -> Result<()> {
        let mut dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.push_prev(dht.id, prev);
        assert_eq!(relay.method, MessageRelayMethod::REPORT);
        // if successor: predecessor is between (id, successor]
        // then update local successor
        dht.successor.update(msg.id);
        Ok(())
    }
}
