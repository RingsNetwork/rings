use crate::dht::{Chord, ChordStablize, Did, PeerRingAction, PeerRingRemoteAction};
use crate::err::{Error, Result};
use crate::message::payload::{MessageRelay, MessageRelayMethod};
use crate::message::protocol::MessageSessionRelayProtocol;
use crate::message::types::{
    AlreadyConnected, ConnectNodeReport, ConnectNodeSend, FindSuccessorReport, FindSuccessorSend,
    JoinDHT, Message, NotifyPredecessorReport, NotifyPredecessorSend,
};
use crate::message::LeaveDHT;
use crate::message::MessageHandler;
use crate::message::OriginVerificationGen;
use crate::prelude::RTCSdpType;
use crate::swarm::TransportManager;
use crate::types::ice_transport::IceTrickleScheme;
use async_trait::async_trait;
use std::str::FromStr;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TChordConnection {
    async fn join_chord(&self, relay: MessageRelay<Message>, prev: Did, msg: JoinDHT)
        -> Result<()>;

    async fn leave_chord(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
        msg: LeaveDHT,
    ) -> Result<()>;

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
    async fn leave_chord(
        &self,
        _relay: MessageRelay<Message>,
        _prev: Did,
        msg: LeaveDHT,
    ) -> Result<()> {
        let mut dht = self.dht.lock().await;
        dht.remove(msg.id);
        Ok(())
    }

    async fn join_chord(
        &self,
        _relay: MessageRelay<Message>,
        prev: Did,
        msg: JoinDHT,
    ) -> Result<()> {
        // here is two situation.
        // finger table just have no other node(beside next), it will be a `create` op
        // otherwise, it will be a `send` op
        let mut dht = self.dht.lock().await;
        match dht.join(msg.id) {
            PeerRingAction::None => Ok(()),
            PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessor(id)) => {
                // if there is only two nodes A, B, it may cause recursion
                // A.successor == B
                // B.successor == A
                // A.find_successor(B)
                if next != prev {
                    self.send_message(
                        &next.into(),
                        // to
                        Some(vec![next].into()),
                        // from
                        Some(vec![dht.id].into()),
                        MessageRelayMethod::SEND,
                        OriginVerificationGen::Origin,
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
        if dht.id != msg.target_id {
            let next_node = match dht.find_successor(msg.target_id)? {
                PeerRingAction::Some(node) => Some(node),
                PeerRingAction::RemoteAction(node, _) => Some(node),
                _ => None,
            }
            .ok_or(Error::MessageHandlerMissNextNode)?;
            relay.relay(dht.id, Some(next_node));
            return self
                .send_message(
                    &next_node,
                    Some(relay.to_path),
                    Some(relay.from_path),
                    MessageRelayMethod::SEND,
                    OriginVerificationGen::Stick(relay.origin_verification),
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
                    .get_handshake_info(&self.swarm.session_manager, RTCSdpType::Answer)
                    .await?
                    .to_string();
                self.send_message(
                    &prev.into(),
                    Some(relay.to_path),
                    Some(relay.from_path),
                    MessageRelayMethod::REPORT,
                    OriginVerificationGen::Origin,
                    Message::ConnectNodeReport(ConnectNodeReport {
                        answer_id: dht.id,
                        transport_uuid: msg.transport_uuid.clone(),
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
                    OriginVerificationGen::Origin,
                    Message::AlreadyConnected(AlreadyConnected { answer_id: dht.id }),
                )
                .await
            }
        }
    }

    async fn connected_node(
        &self,
        relay: MessageRelay<Message>,
        _prev: Did,
        msg: ConnectNodeReport,
    ) -> Result<()> {
        let dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.relay(dht.id, None);
        if let Some(prev_node) = relay.next() {
            self.send_message(
                &prev_node,
                Some(relay.to_path),
                Some(relay.from_path),
                MessageRelayMethod::REPORT,
                OriginVerificationGen::Stick(relay.origin_verification),
                Message::ConnectNodeReport(msg.clone()),
            )
            .await
        } else {
            let transport = self
                .swarm
                .find_pending_transport(
                    uuid::Uuid::from_str(&msg.transport_uuid)
                        .map_err(|_| Error::InvalidTransportUuid)?,
                )?
                .ok_or(Error::MessageHandlerMissTransportConnectedNode)?;
            transport
                .register_remote_info(msg.handshake_info.clone().into())
                .await?;
            self.swarm.register(&msg.answer_id, transport).await
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
                OriginVerificationGen::Stick(relay.origin_verification),
                Message::AlreadyConnected(msg.clone()),
            )
            .await
        } else {
            self.swarm
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
                    OriginVerificationGen::Origin,
                    Message::FindSuccessorReport(FindSuccessorReport {
                        id,
                        for_fix: msg.for_fix,
                    }),
                )
                .await
            }
            PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessor(id)) => {
                relay.relay(dht.id, Some(next));
                self.send_message(
                    &next.into(),
                    Some(relay.to_path),
                    Some(relay.from_path),
                    MessageRelayMethod::SEND,
                    OriginVerificationGen::Origin,
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
        _prev: Did,
        msg: FindSuccessorReport,
    ) -> Result<()> {
        let mut dht = self.dht.lock().await;
        let mut relay = relay.clone();
        relay.relay(dht.id, None);
        if let Some(next) = relay.next() {
            self.send_message(
                &next.into(),
                Some(relay.to_path),
                Some(relay.from_path),
                MessageRelayMethod::REPORT,
                OriginVerificationGen::Stick(relay.origin_verification),
                Message::FindSuccessorReport(msg.clone()),
            )
            .await
        } else {
            if self.swarm.get_transport(&msg.id).is_none() && msg.id != self.swarm.address().into()
            {
                return self.connect(&msg.id.into()).await;
            }
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
            Some(relay.to_path),
            Some(relay.from_path),
            MessageRelayMethod::REPORT,
            OriginVerificationGen::Origin,
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

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod test {
    use super::*;
    use crate::dht::PeerRing;
    use crate::ecc::SecretKey;
    use crate::message::MessageHandler;
    use crate::prelude::RTCSdpType;
    use crate::session::SessionManager;
    use crate::swarm::Swarm;
    use crate::swarm::TransportManager;
    use crate::types::ice_transport::IceTrickleScheme;
    use futures::lock::Mutex;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_triple_node() -> Result<()> {
        let stun = "stun://stun.l.google.com:19302";

        let mut key1 = SecretKey::random();
        let mut key2 = SecretKey::random();
        let mut key3 = SecretKey::random();

        let mut v = vec![key1, key2, key3];

        v.sort_by(|a, b| {
            if a.address() < b.address() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        });
        (key1, key2, key3) = (v[0], v[1], v[2]);

        println!(
            "test with key1: {:?}, key2: {:?}, key3: {:?}",
            key1.address(),
            key2.address(),
            key3.address()
        );

        let dht1 = Arc::new(Mutex::new(PeerRing::new(key1.address().into())));
        let dht2 = Arc::new(Mutex::new(PeerRing::new(key2.address().into())));
        let dht3 = Arc::new(Mutex::new(PeerRing::new(key3.address().into())));

        let sm1 = SessionManager::new_with_seckey(&key1).unwrap();
        let sm2 = SessionManager::new_with_seckey(&key2).unwrap();
        let sm3 = SessionManager::new_with_seckey(&key3).unwrap();

        let swarm1 = Arc::new(Swarm::new(stun, key1.address(), sm1.clone()));
        let swarm2 = Arc::new(Swarm::new(stun, key2.address(), sm2.clone()));
        let swarm3 = Arc::new(Swarm::new(stun, key3.address(), sm3.clone()));

        let transport1 = swarm1.new_transport().await.unwrap();
        let transport2 = swarm2.new_transport().await.unwrap();
        let transport3 = swarm3.new_transport().await.unwrap();

        let node1 = MessageHandler::new(Arc::clone(&dht1), Arc::clone(&swarm1));
        let node2 = MessageHandler::new(Arc::clone(&dht2), Arc::clone(&swarm2));
        let node3 = MessageHandler::new(Arc::clone(&dht3), Arc::clone(&swarm3));

        // now we connect node1 and node2

        let handshake_info1 = transport1
            .get_handshake_info(&sm1, RTCSdpType::Offer)
            .await?;

        let addr1 = transport2.register_remote_info(handshake_info1).await?;

        let handshake_info2 = transport2
            .get_handshake_info(&sm2, RTCSdpType::Answer)
            .await?;

        let addr2 = transport1.register_remote_info(handshake_info2).await?;

        assert_eq!(addr1, key1.address());
        assert_eq!(addr2, key2.address());
        let promise_1 = transport1.connect_success_promise().await?;
        let promise_2 = transport2.connect_success_promise().await?;
        promise_1.await?;
        promise_2.await?;

        swarm1
            .register(&swarm2.address(), transport1.clone())
            .await
            .unwrap();
        swarm2
            .register(&swarm1.address(), transport2.clone())
            .await
            .unwrap();

        assert!(swarm1.get_transport(&key2.address()).is_some());
        assert!(swarm2.get_transport(&key1.address()).is_some());

        // JoinDHT
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(&ev_1.from_path.clone(), &vec![key1.address().into()]);
        assert_eq!(&ev_1.to_path.clone(), &vec![key1.address().into()]);
        if let Message::JoinDHT(x) = ev_1.data {
            assert_eq!(x.id, key2.address().into());
        } else {
            panic!();
        }
        // the message is send from key1
        // will be transform into some remote action
        assert_eq!(&ev_1.addr, &key1.address());

        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(&ev_2.from_path.clone(), &vec![key2.address().into()]);
        assert_eq!(&ev_2.to_path.clone(), &vec![key2.address().into()]);
        if let Message::JoinDHT(x) = ev_2.data {
            assert_eq!(x.id, key1.address().into());
        } else {
            panic!();
        }
        // the message is send from key2
        // will be transform into some remote action
        assert_eq!(&ev_2.addr, &key2.address());

        let ev_1 = node1.listen_once().await.unwrap();
        // msg is send from key2
        assert_eq!(&ev_1.addr, &key2.address());
        assert_eq!(&ev_1.from_path.clone(), &vec![key2.address().into()]);
        assert_eq!(&ev_1.to_path.clone(), &vec![key1.address().into()]);
        if let Message::FindSuccessorSend(x) = ev_1.data {
            assert_eq!(x.id, key2.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(&ev_2.addr, &key1.address());
        assert_eq!(&ev_2.from_path.clone(), &vec![key1.address().into()]);
        assert_eq!(&ev_2.to_path.clone(), &vec![key2.address().into()]);
        if let Message::FindSuccessorSend(x) = ev_2.data {
            assert_eq!(x.id, key1.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        // node2 response self as node1's successor
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(&ev_1.addr, &key2.address());
        assert_eq!(&ev_1.from_path.clone(), &vec![key1.address().into()]);
        assert_eq!(&ev_1.to_path.clone(), &vec![key2.address().into()]);
        if let Message::FindSuccessorReport(x) = ev_1.data {
            // for node2 there is no did is more closer to key1, so it response key1
            // and dht1 wont update
            assert!(!dht1
                .lock()
                .await
                .successor
                .list()
                .contains(&key1.address().into()));
            assert_eq!(x.id, key1.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        // key1 response self as key2's successor
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(&ev_2.addr, &key1.address());
        assert_eq!(&ev_2.from_path.clone(), &vec![key2.address().into()]);
        assert_eq!(&ev_2.to_path.clone(), &vec![key1.address().into()]);
        if let Message::FindSuccessorReport(x) = ev_2.data {
            // for key1 there is no did is more closer to key1, so it response key1
            // and dht2 wont update
            assert_eq!(x.id, key2.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }
        assert!(!dht2
            .lock()
            .await
            .successor
            .list()
            .contains(&key2.address().into()));

        println!("========================================");
        println!("||  now we start join node3 to node2   ||");
        println!("========================================");

        let handshake_info3 = transport3
            .get_handshake_info(&sm3, RTCSdpType::Offer)
            .await?;
        // created a new transport
        let transport2 = swarm2.new_transport().await.unwrap();

        let addr3 = transport2.register_remote_info(handshake_info3).await?;

        assert_eq!(addr3, key3.address());

        let handshake_info2 = transport2
            .get_handshake_info(&sm2, RTCSdpType::Answer)
            .await?;

        let addr2 = transport3.register_remote_info(handshake_info2).await?;

        assert_eq!(addr2, key2.address());

        let promise_3 = transport3.connect_success_promise().await?;
        let promise_2 = transport2.connect_success_promise().await?;
        promise_3.await?;
        promise_2.await?;

        swarm2
            .register(&swarm3.address(), transport2.clone())
            .await
            .unwrap();

        swarm3
            .register(&swarm2.address(), transport3.clone())
            .await
            .unwrap();

        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(&ev_3.addr, &key3.address());
        assert_eq!(&ev_3.from_path.clone(), &vec![key3.address().into()]);
        assert_eq!(&ev_3.to_path.clone(), &vec![key3.address().into()]);
        if let Message::JoinDHT(x) = ev_3.data {
            assert_eq!(x.id, key2.address().into());
        } else {
            panic!();
        }

        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(&ev_2.addr, &key2.address());
        assert_eq!(&ev_2.from_path.clone(), &vec![key2.address().into()]);
        assert_eq!(&ev_2.to_path.clone(), &vec![key2.address().into()]);
        if let Message::JoinDHT(x) = ev_2.data {
            assert_eq!(x.id, key3.address().into());
        } else {
            panic!();
        }

        let ev_3 = node3.listen_once().await.unwrap();
        // msg is send from node2
        assert_eq!(&ev_3.addr, &key2.address());
        assert_eq!(&ev_3.from_path.clone(), &vec![key2.address().into()]);
        assert_eq!(&ev_3.to_path.clone(), &vec![key3.address().into()]);
        if let Message::FindSuccessorSend(x) = ev_3.data {
            assert_eq!(x.id, key2.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(&ev_2.addr, &key3.address());
        assert_eq!(&ev_2.from_path.clone(), &vec![key3.address().into()]);
        assert_eq!(&ev_2.to_path.clone(), &vec![key2.address().into()]);
        if let Message::FindSuccessorSend(x) = ev_2.data {
            assert_eq!(x.id, key3.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        // node2 response self as node1's successor
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(&ev_3.addr, &key2.address());
        assert_eq!(&ev_3.from_path.clone(), &vec![key3.address().into()]);
        assert_eq!(&ev_3.to_path.clone(), &vec![key2.address().into()]);
        if let Message::FindSuccessorReport(x) = ev_3.data {
            // for node2 there is no did is more closer to key3, so it response key3
            // and dht3 wont update
            assert!(!dht3
                .lock()
                .await
                .successor
                .list()
                .contains(&key3.address().into()));
            assert_eq!(x.id, key3.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        // key3 response self as key2's successor
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(&ev_2.addr, &key3.address());
        assert_eq!(&ev_2.from_path.clone(), &vec![key2.address().into()]);
        assert_eq!(&ev_2.to_path.clone(), &vec![key3.address().into()]);
        if let Message::FindSuccessorReport(x) = ev_2.data {
            // for key3 there is no did is more closer to key3, so it response key3
            // and dht2 wont update
            assert_eq!(x.id, key2.address().into());
            assert!(!dht2
                .lock()
                .await
                .successor
                .list()
                .contains(&key2.address().into()));
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        println!("=======================================================");
        println!("||  now we connect join node3 to node1 via DHT       ||");
        println!("=======================================================");

        // node1's successor is node2
        assert!(swarm1.get_transport(&key3.address()).is_none());
        assert_eq!(
            node1.dht.lock().await.successor.max(),
            key2.address().into()
        );
        node1.connect(&key3.address()).await.unwrap();
        let ev2 = node2.listen_once().await.unwrap();

        // msg is send from node 1 to node 2
        assert_eq!(&ev2.addr, &key1.address());
        assert_eq!(&ev2.to_path.clone(), &vec![key2.address().into()]);
        assert_eq!(&ev2.from_path.clone(), &vec![key1.address().into()]);

        if let Message::ConnectNodeSend(x) = ev2.data {
            assert_eq!(x.target_id, key3.address().into());
            assert_eq!(x.sender_id, key1.address().into());
        } else {
            panic!();
        }

        let ev3 = node3.listen_once().await.unwrap();

        // msg is relayed from node 2 to node 3
        println!(
            "test with key1: {:?}, key2: {:?}, key3: {:?}",
            key1.address(),
            key2.address(),
            key3.address()
        );

        assert_eq!(&ev3.addr, &key2.address());
        assert_eq!(
            &ev3.to_path.clone(),
            &vec![key3.address().into()],
            "to_path not match!"
        );
        assert_eq!(
            &ev3.from_path.clone(),
            &vec![key1.address().into(), key2.address().into()]
        );
        if let Message::ConnectNodeSend(x) = ev3.data {
            assert_eq!(x.target_id, key3.address().into());
            assert_eq!(x.sender_id, key1.address().into());
        } else {
            panic!();
        }

        let ev2 = node2.listen_once().await.unwrap();
        // node3 send report to node2
        // for a report the to_path should as same as a send request
        assert_eq!(&ev2.addr, &key3.address());
        assert_eq!(
            &ev2.from_path.clone(),
            &vec![key1.address().into(), key2.address().into()]
        );
        assert_eq!(&ev2.to_path.clone(), &vec![key3.address().into()]);
        if let Message::ConnectNodeReport(x) = ev2.data {
            assert_eq!(x.answer_id, key3.address().into());
        } else {
            panic!();
        }
        // node 2 send report to node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(&ev1.addr, &key2.address());
        assert_eq!(&ev1.from_path, &vec![key1.address().into()]);
        assert_eq!(
            &ev1.to_path,
            &vec![key2.address().into(), key3.address().into()]
        );
        if let Message::ConnectNodeReport(x) = ev1.data {
            assert_eq!(x.answer_id, key3.address().into());
        } else {
            panic!();
        }
        assert!(swarm1.get_transport(&key3.address()).is_some());
        Ok(())
    }

    /// We have three nodes, where
    /// key 1 > key2 > key3
    /// we connect key1 to key3 first
    /// then when key1 send `FindSuccessor` to key3
    /// and when stablization
    /// key3 should response key2 to key1
    /// key1 should noti key3 that
    /// key3's precessor is key1
    #[tokio::test]
    async fn test_stablize() -> Result<()> {
        let stun = "stun://stun.l.google.com:19302";

        let mut key1 = SecretKey::random();
        let mut key2 = SecretKey::random();
        let mut key3 = SecretKey::random();

        let mut v = vec![key1, key2, key3];

        v.sort_by(|a, b| {
            if a.address() < b.address() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        });
        (key1, key2, key3) = (v[0], v[1], v[2]);

        println!(
            "test with key1: {:?}, key2: {:?}, key3: {:?}",
            key1.address(),
            key2.address(),
            key3.address()
        );

        let dht1 = Arc::new(Mutex::new(PeerRing::new(key1.address().into())));
        let dht2 = Arc::new(Mutex::new(PeerRing::new(key2.address().into())));
        let dht3 = Arc::new(Mutex::new(PeerRing::new(key3.address().into())));

        let sm1 = SessionManager::new_with_seckey(&key1).unwrap();
        let sm2 = SessionManager::new_with_seckey(&key2).unwrap();
        let sm3 = SessionManager::new_with_seckey(&key3).unwrap();

        let swarm1 = Arc::new(Swarm::new(stun, key1.address(), sm1.clone()));
        let swarm2 = Arc::new(Swarm::new(stun, key2.address(), sm2.clone()));
        let swarm3 = Arc::new(Swarm::new(stun, key3.address(), sm3.clone()));

        let transport1 = swarm1.new_transport().await.unwrap();
        let transport2 = swarm2.new_transport().await.unwrap();
        let transport3 = swarm3.new_transport().await.unwrap();

        let node1 = MessageHandler::new(Arc::clone(&dht1), Arc::clone(&swarm1));
        let node2 = MessageHandler::new(Arc::clone(&dht2), Arc::clone(&swarm2));
        let node3 = MessageHandler::new(Arc::clone(&dht3), Arc::clone(&swarm3));

        // now we connect node1 and node3
        // first node1 generate handshake info
        let handshake_info1 = transport1
            .get_handshake_info(&sm1, RTCSdpType::Offer)
            .await?;

        // node3 register handshake from node1
        let addr1 = transport3.register_remote_info(handshake_info1).await?;
        // and reponse a Answer
        let handshake_info3 = transport3
            .get_handshake_info(&sm3, RTCSdpType::Answer)
            .await?;

        // node1 accpeted the answer
        let addr3 = transport1.register_remote_info(handshake_info3).await?;

        assert_eq!(addr1, key1.address());
        assert_eq!(addr3, key3.address());
        // wait until ICE finish
        let promise_1 = transport1.connect_success_promise().await?;
        let promise_3 = transport3.connect_success_promise().await?;
        promise_1.await?;
        promise_3.await?;
        // thus register transport to swarm
        swarm1
            .register(&swarm3.address(), transport1.clone())
            .await
            .unwrap();
        swarm3
            .register(&swarm1.address(), transport3.clone())
            .await
            .unwrap();

        // node1 and node3 will gen JoinDHT Event
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(&ev_1.from_path.clone(), &vec![key1.address().into()]);
        assert_eq!(&ev_1.to_path.clone(), &vec![key1.address().into()]);
        assert_eq!(&ev_1.addr, &key1.address());

        if let Message::JoinDHT(x) = ev_1.data {
            assert_eq!(x.id, key3.address().into());
        } else {
            panic!();
        }
        // the message is send from key1
        // will be transform into some remote action

        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(&ev_3.from_path.clone(), &vec![key3.address().into()]);
        assert_eq!(&ev_3.to_path.clone(), &vec![key3.address().into()]);
        assert_eq!(&ev_3.addr, &key3.address());

        if let Message::JoinDHT(x) = ev_3.data {
            assert_eq!(x.id, key1.address().into());
        } else {
            panic!();
        }

        let ev_1 = node1.listen_once().await.unwrap();
        // msg is send from key3
        assert_eq!(&ev_1.addr, &key3.address());
        assert_eq!(&ev_1.from_path.clone(), &vec![key3.address().into()]);
        assert_eq!(&ev_1.to_path.clone(), &vec![key1.address().into()]);
        if let Message::FindSuccessorSend(x) = ev_1.data {
            assert_eq!(x.id, key3.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(&ev_3.addr, &key1.address());
        assert_eq!(&ev_3.from_path.clone(), &vec![key1.address().into()]);
        assert_eq!(&ev_3.to_path.clone(), &vec![key3.address().into()]);
        if let Message::FindSuccessorSend(x) = ev_3.data {
            assert_eq!(x.id, key1.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        // node3 response self as node1's successor
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(&ev_1.addr, &key3.address());
        assert_eq!(&ev_1.from_path.clone(), &vec![key1.address().into()]);
        assert_eq!(&ev_1.to_path.clone(), &vec![key3.address().into()]);
        if let Message::FindSuccessorReport(x) = ev_1.data {
            // for node3 there is no did is more closer to key1, so it response key1
            // and dht1 wont update
            assert!(!dht1
                .lock()
                .await
                .successor
                .list()
                .contains(&key1.address().into()));
            assert_eq!(x.id, key1.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        // key1 response self as key3's successor
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(&ev_3.addr, &key1.address());
        assert_eq!(&ev_3.from_path.clone(), &vec![key3.address().into()]);
        assert_eq!(&ev_3.to_path.clone(), &vec![key1.address().into()]);
        if let Message::FindSuccessorReport(x) = ev_3.data {
            // for key1 there is no did is more closer to key1, so it response key1
            // and dht3 wont update
            assert_eq!(x.id, key3.address().into());
            assert!(!dht3
                .lock()
                .await
                .successor
                .list()
                .contains(&key3.address().into()));
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        println!("=======================================================");
        println!("||  now we connect node2 to node3       ||");
        println!("=======================================================");
        // now we connect node2 and node3
        // first node2 generate handshake info
        let transport3 = swarm3.new_transport().await.unwrap();
        assert!(swarm2.get_transport(&key3.address()).is_none());

        let handshake_info2 = transport2
            .get_handshake_info(&sm2, RTCSdpType::Offer)
            .await?;

        // node3 register handshake from node2
        let addr2 = transport3.register_remote_info(handshake_info2).await?;
        // and reponse a Answer
        let handshake_info3 = transport3
            .get_handshake_info(&sm3, RTCSdpType::Answer)
            .await?;

        // node2 accpeted the answer
        let addr3 = transport2.register_remote_info(handshake_info3).await?;

        assert_eq!(addr2, key2.address());
        assert_eq!(addr3, key3.address());
        // wait until ICE finish
        let promise_2 = transport2.connect_success_promise().await?;
        let promise_3 = transport3.connect_success_promise().await?;
        promise_2.await?;
        promise_3.await?;
        // thus register transport to swarm
        swarm2
            .register(&swarm3.address(), transport2.clone())
            .await
            .unwrap();
        swarm3
            .register(&swarm2.address(), transport3.clone())
            .await
            .unwrap();

        // node2 and node3 will gen JoinDHT Event
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(&ev_2.from_path.clone(), &vec![key2.address().into()]);
        assert_eq!(&ev_2.to_path.clone(), &vec![key2.address().into()]);
        assert_eq!(&ev_2.addr, &key2.address());

        if let Message::JoinDHT(x) = ev_2.data {
            assert_eq!(x.id, key3.address().into());
        } else {
            panic!();
        }
        // the message is send from key2
        // will be transform into some remote action

        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(&ev_3.from_path.clone(), &vec![key3.address().into()]);
        assert_eq!(&ev_3.to_path.clone(), &vec![key3.address().into()]);
        assert_eq!(&ev_3.addr, &key3.address());

        if let Message::JoinDHT(x) = ev_3.data {
            assert_eq!(x.id, key2.address().into());
        } else {
            panic!();
        }

        let ev_2 = node2.listen_once().await.unwrap();
        // msg is send from key3
        // node 3 ask node 2 for successor
        assert_eq!(&ev_2.addr, &key3.address());
        assert_eq!(&ev_2.from_path.clone(), &vec![key3.address().into()]);
        assert_eq!(&ev_2.to_path.clone(), &vec![key2.address().into()]);
        if let Message::FindSuccessorSend(x) = ev_2.data {
            assert_eq!(x.id, key3.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        // node 2 ask node 3 for successor
        // node 3 will ask it's successor: node 1
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(&ev_3.addr, &key2.address());
        assert_eq!(&ev_3.from_path.clone(), &vec![key2.address().into()]);
        assert_eq!(&ev_3.to_path.clone(), &vec![key3.address().into()]);
        if let Message::FindSuccessorSend(x) = ev_3.data {
            assert_eq!(x.id, key2.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        // node 2 report to node3
        // node 2 report node2's successor is node 3
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(&ev_3.addr, &key2.address());
        assert_eq!(&ev_3.from_path, &vec![key3.address().into()]);
        assert_eq!(&ev_3.to_path, &vec![key2.address().into()]);
        if let Message::FindSuccessorReport(x) = ev_3.data {
            assert_eq!(x.id, key3.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        // node 1 -> node 2 -> node 3
        // node3's successor is node1,
        // according to Chord algorithm
        // node 3 will ask cloest_preceding_node to find successor of node2
        // where v <- (node3, node2)
        // so node 3 will ask node1 to find successor of node2
        // *BECAUSE* node1, node2, node3, is a *RING*
        // which can also pe present as node3, node1, node1
        // the msg is send from node 3 to node 1
        // from_path: [node2, node3]
        // to_path: [node1]
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(&ev_1.addr, &key3.address());
        assert_eq!(
            &ev_1.from_path,
            &vec![key2.address().into(), key3.address().into()]
        );
        assert_eq!(&ev_1.to_path, &vec![key1.address().into()]);
        if let Message::FindSuccessorSend(x) = ev_1.data {
            assert_eq!(x.id, key2.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        // node 1 report to node3
        // node 1 report node2's successor is node 3
        // because, node2 only know node3
        assert!(!dht1
            .lock()
            .await
            .finger
            .contains(&Some(key2.address().into())));
        // from source of chord:
        //     if self.bias(id) <= self.bias(self.successor.max()) || self.successor.is_none() {
        //          Ok(PeerRingAction::Some(id))
        // node1's successor is node3
        // node2 is in [node1, node3]
        // so it will response node2

        // [node1, node2, node3]
        // from_path: node2, node3
        // to_path: node1
        let ev_3 = node3.listen_once().await.unwrap();
        assert_eq!(&ev_3.addr, &key1.address());
        assert_eq!(
            &ev_3.from_path,
            &vec![key2.address().into(), key3.address().into()]
        );
        assert_eq!(&ev_3.to_path, &vec![key1.address().into()]);
        if let Message::FindSuccessorReport(x) = ev_3.data {
            assert_eq!(x.id, key2.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        // node3 report it's result to node 2
        // path is: node 2 -> node3 -> node1 -> node3 -> node2
        // from_path: [node2],
        // to_path: [node3, node1]
        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(&ev_2.addr, &key3.address());

        // from_path should be node2
        assert_eq!(&ev_2.from_path, &vec![key2.address().into()]);
        // to_path should be node3 node 1
        assert_eq!(
            &ev_2.to_path,
            &vec![key3.address().into(), key1.address().into()]
        );

        if let Message::FindSuccessorReport(x) = ev_2.data {
            assert_eq!(x.id, key2.address().into());
            assert!(!x.for_fix);
        } else {
            panic!();
        }

        // now node1's successor is node3,
        // node2's successor is node 3
        // node3's successor is node 1
        assert_eq!(
            dht1.lock().await.successor.list(),
            vec![key3.address().into()]
        );
        assert_eq!(
            dht2.lock().await.successor.list(),
            vec![key3.address().into()]
        );
        assert_eq!(
            dht3.lock().await.successor.list(),
            vec![key1.address().into()]
        );

        Ok(())
    }
}
