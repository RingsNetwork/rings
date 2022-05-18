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
                        Some(vec![next.into()].into()),
                        // from
                        Some(vec![dht.id.into()].into()),
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
                    Some(relay.to_path),
                    Some(relay.from_path),
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
        relay.relay(dht.id, None);
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
        relay.relay(dht.id, None);
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
            Some(relay.to_path),
            Some(relay.from_path),
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

        let session1 = SessionManager::new_with_seckey(&key1).unwrap();
        let session2 = SessionManager::new_with_seckey(&key2).unwrap();
        let session3 = SessionManager::new_with_seckey(&key3).unwrap();

        let swarm1 = Arc::new(Swarm::new(stun, key1.address(), session1.clone()));
        let swarm2 = Arc::new(Swarm::new(stun, key2.address(), session2.clone()));
        let swarm3 = Arc::new(Swarm::new(stun, key3.address(), session3.clone()));

        let transport1 = swarm1.new_transport().await.unwrap();
        let transport2 = swarm2.new_transport().await.unwrap();
        let transport3 = swarm3.new_transport().await.unwrap();

        let node1 = MessageHandler::new(Arc::clone(&dht1), Arc::clone(&swarm1));
        let node2 = MessageHandler::new(Arc::clone(&dht2), Arc::clone(&swarm2));
        let node3 = MessageHandler::new(Arc::clone(&dht3), Arc::clone(&swarm3));

        // now we connect node1 and node2

        let handshake_info1 = transport1
            .get_handshake_info(session1.clone(), RTCSdpType::Offer)
            .await?;

        let addr1 = transport2.register_remote_info(handshake_info1).await?;

        let handshake_info2 = transport2
            .get_handshake_info(session2.clone(), RTCSdpType::Answer)
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
        // JoinDHT
        let ev_1 = node1.listen_once().await.unwrap();
        assert_eq!(&ev_1.from_path.clone(), &vec![key1.address().into()]);
        assert_eq!(&ev_1.to_path.clone(), &vec![key1.address().into()]);
        if let Message::JoinDHT(x) = ev_1.data {
            assert_eq!(x.id, key2.address().into());
        } else {
            assert!(false);
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
            assert!(false);
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
            assert_eq!(x.for_fix, false);
        } else {
            assert!(false);
        }

        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(&ev_2.addr, &key1.address());
        assert_eq!(&ev_2.from_path.clone(), &vec![key1.address().into()]);
        assert_eq!(&ev_2.to_path.clone(), &vec![key2.address().into()]);
        if let Message::FindSuccessorSend(x) = ev_2.data {
            assert_eq!(x.id, key1.address().into());
            assert_eq!(x.for_fix, false);
        } else {
            assert!(false);
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
            assert_eq!(x.for_fix, false);
        } else {
            assert!(false);
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
            assert!(!dht2
                .lock()
                .await
                .successor
                .list()
                .contains(&key2.address().into()));
            assert_eq!(x.for_fix, false);
        } else {
            assert!(false);
        }

        println!("========================================");
        println!("||  now we start join node3 to node2   ||");
        println!("========================================");

        let handshake_info3 = transport3
            .get_handshake_info(session3, RTCSdpType::Offer)
            .await?;
        // created a new transport
        let transport2 = swarm2.new_transport().await.unwrap();

        let addr3 = transport2.register_remote_info(handshake_info3).await?;

        assert_eq!(addr3, key3.address());

        let handshake_info2 = transport2
            .get_handshake_info(session2, RTCSdpType::Answer)
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
            assert!(false);
        }

        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(&ev_2.addr, &key2.address());
        assert_eq!(&ev_2.from_path.clone(), &vec![key2.address().into()]);
        assert_eq!(&ev_2.to_path.clone(), &vec![key2.address().into()]);
        if let Message::JoinDHT(x) = ev_2.data {
            assert_eq!(x.id, key3.address().into());
        } else {
            assert!(false);
        }

        let ev_3 = node3.listen_once().await.unwrap();
        // msg is send from node2
        assert_eq!(&ev_3.addr, &key2.address());
        assert_eq!(&ev_3.from_path.clone(), &vec![key2.address().into()]);
        assert_eq!(&ev_3.to_path.clone(), &vec![key3.address().into()]);
        if let Message::FindSuccessorSend(x) = ev_3.data {
            assert_eq!(x.id, key2.address().into());
            assert_eq!(x.for_fix, false);
        } else {
            assert!(false);
        }

        let ev_2 = node2.listen_once().await.unwrap();
        assert_eq!(&ev_2.addr, &key3.address());
        assert_eq!(&ev_2.from_path.clone(), &vec![key3.address().into()]);
        assert_eq!(&ev_2.to_path.clone(), &vec![key2.address().into()]);
        if let Message::FindSuccessorSend(x) = ev_2.data {
            assert_eq!(x.id, key3.address().into());
            assert_eq!(x.for_fix, false);
        } else {
            assert!(false);
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
            assert_eq!(x.for_fix, false);
        } else {
            assert!(false);
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
            assert_eq!(x.for_fix, false);
        } else {
            assert!(false);
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
        node1.connect(key3.address()).await.unwrap();
        let ev2 = node2.listen_once().await.unwrap();

        // msg is send from node 1 to node 2
        assert_eq!(&ev2.addr, &key1.address());
        assert_eq!(&ev2.to_path.clone(), &vec![key2.address().into()]);
        assert_eq!(&ev2.from_path.clone(), &vec![key1.address().into()]);

        if let Message::ConnectNodeSend(x) = ev2.data {
            assert_eq!(x.target_id, key3.address().into());
            assert_eq!(x.sender_id, key1.address().into());
        } else {
            assert!(false);
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
            assert!(false);
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
            assert!(false);
        }
        // node 2 send report to node1
        let ev1 = node1.listen_once().await.unwrap();
        assert_eq!(&ev1.addr, &key2.address());
        assert_eq!(&ev1.from_path.clone(), &vec![key1.address().into()]);
        assert_eq!(
            &ev1.to_path.clone(),
            &vec![key2.address().into(), key3.address().into()]
        );
        if let Message::ConnectNodeReport(x) = ev1.data {
            assert_eq!(x.answer_id, key3.address().into());
        } else {
            assert!(false);
        }
        assert!(swarm1.get_transport(&key3.address()).is_some());
        Ok(())
    }
}
