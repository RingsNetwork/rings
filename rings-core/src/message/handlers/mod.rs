use crate::dht::{Did, PeerRing};
use crate::err::{Error, Result};
use crate::message::payload::{MessageRelay, MessageRelayMethod};
use crate::message::types::Message;
use crate::swarm::Swarm;
use crate::swarm::TransportManager;
use crate::types::ice_transport::IceTrickleScheme;
use async_recursion::async_recursion;
use futures::lock::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use web3::types::Address;

#[cfg(feature = "wasm")]
use web_sys::RtcSdpType;
#[cfg(not(feature = "wasm"))]
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;

pub mod connection;
pub mod storage;

use connection::TChordConnection;
use storage::TChordStorage;

#[cfg(not(feature = "wasm"))]
type CallbackFn = Box<dyn FnMut(&MessageRelay<Message>, Did) -> Result<()> + Send + Sync>;

#[cfg(feature = "wasm")]
type CallbackFn = Box<dyn FnMut(&MessageRelay<Message>, Did) -> Result<()>>;

#[derive(Clone)]
pub struct MessageHandler {
    dht: Arc<Mutex<PeerRing>>,
    swarm: Arc<Swarm>,
    callback: Arc<Mutex<Option<CallbackFn>>>,
}

impl MessageHandler {
    pub fn new_with_callback(
        dht: Arc<Mutex<PeerRing>>,
        swarm: Arc<Swarm>,
        callback: CallbackFn,
    ) -> Self {
        Self {
            dht,
            swarm,
            callback: Arc::new(Mutex::new(Some(callback))),
        }
    }

    pub fn new(dht: Arc<Mutex<PeerRing>>, swarm: Arc<Swarm>) -> Self {
        Self {
            dht,
            swarm,
            callback: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn set_callback(&self, f: CallbackFn) {
        let mut cb = self.callback.lock().await;
        *cb = Some(f)
    }

    pub async fn send_relay_message(
        &self,
        address: &Address,
        msg: MessageRelay<Message>,
    ) -> Result<()> {
        println!("+++++++++++++++++++++++++++++++++");
        println!("node {:?}", self.swarm.address());
        println!("Sent {:?}", msg.clone());
        println!("node {:?}", address);
        println!("+++++++++++++++++++++++++++++++++");

        self.swarm.send_message(address, msg).await
    }

    pub async fn send_message_default(&self, address: &Address, message: Message) -> Result<()> {
        self.send_message(address, None, None, MessageRelayMethod::SEND, message)
            .await
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
        let payload = MessageRelay::new(
            message,
            &self.swarm.session(),
            None,
            to_path,
            from_path,
            method,
        )?;
        self.send_relay_message(address, payload).await
    }

    pub async fn connect(&self, address: Address) -> Result<()> {
        let transport = self.swarm.new_transport().await?;
        let handshake_info = transport
            .get_handshake_info(self.swarm.session().clone(), RTCSdpType::Offer)
            .await?;
        let connect_msg = Message::ConnectNodeSend(super::ConnectNodeSend {
            sender_id: self.swarm.address().into(),
            target_id: address.into(),
            handshake_info: handshake_info.to_string(),
        });
        let target = self.dht.lock().await.successor.max();
        self.send_message(
            &target,
            // to_path
            Some(vec![target].into()),
            // from_path
            Some(vec![self.swarm.address().into()].into()),
            MessageRelayMethod::SEND,
            connect_msg
        ).await
    }

    #[cfg_attr(feature = "wasm", async_recursion(?Send))]
    #[cfg_attr(not(feature = "wasm"), async_recursion)]
    pub async fn handle_message_relay(
        &self,
        relay: MessageRelay<Message>,
        prev: Did,
    ) -> Result<()> {
        let data = relay.data.clone();
        match data {
            Message::JoinDHT(msg) => self.join_chord(relay.clone(), prev, msg).await,
            Message::ConnectNodeSend(msg) => self.connect_node(relay.clone(), prev, msg).await,
            Message::ConnectNodeReport(msg) => self.connected_node(relay.clone(), prev, msg).await,
            Message::AlreadyConnected(msg) => {
                self.already_connected(relay.clone(), prev, msg).await
            }
            Message::FindSuccessorSend(msg) => self.find_successor(relay.clone(), prev, msg).await,
            Message::FindSuccessorReport(msg) => {
                self.found_successor(relay.clone(), prev, msg).await
            }
            Message::NotifyPredecessorSend(msg) => {
                self.notify_predecessor(relay.clone(), prev, msg).await
            }
            Message::NotifyPredecessorReport(msg) => {
                self.notified_predecessor(relay.clone(), prev, msg).await
            }
            Message::SearchVNode(msg) => self.search_vnode(relay.clone(), prev, msg).await,
            Message::FoundVNode(msg) => self.found_vnode(relay.clone(), prev, msg).await,
            Message::StoreVNode(msg) => self.store_vnode(relay.clone(), prev, msg).await,
            Message::MultiCall(msg) => {
                for message in msg.messages {
                    let payload = MessageRelay::new(
                        message.clone(),
                        &self.swarm.session(),
                        None,
                        Some(relay.to_path.clone()),
                        Some(relay.from_path.clone()),
                        relay.method.clone(),
                    )?;
                    self.handle_message_relay(payload, prev).await.unwrap_or(());
                }
                Ok(())
            }
            Message::CustomMessage(_) => Ok(()),
            x => Err(Error::MessageHandlerUnsupportMessageType(format!(
                "{:?}",
                x
            ))),
        }?;
        let mut callback = self.callback.lock().await;
        if let Some(ref mut cb) = *callback {
            cb(&relay, prev)?;
        }
        Ok(())
    }

    /// This method is required because web-sys components is not `Send`
    /// which means a listening loop cannot running concurrency.
    pub async fn listen_once(&self) -> Option<MessageRelay<Message>> {
        if let Some(relay_message) = self.swarm.poll_message().await {
            if !relay_message.verify() {
                log::error!("Cannot verify msg or it's expired: {:?}", relay_message);
            }
            let addr = relay_message.addr.into();
            if let Err(e) = self.handle_message_relay(relay_message.clone(), addr).await {
                log::error!("Error in handle_message: {}", e);
            }
            Some(relay_message)
        } else {
            None
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
                let addr = relay_message.addr.into();
                if let Err(e) = self.handle_message_relay(relay_message, addr).await {
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
    use wasm_bindgen_futures::spawn_local;

    #[async_trait(?Send)]
    impl MessageListener for MessageHandler {
        async fn listen(self: Arc<Self>) {
            let handler = Arc::clone(&self);
            let func = move || {
                let handler = Arc::clone(&handler);
                spawn_local(Box::pin(async move {
                    handler.listen_once().await;
                }));
            };
            poll!(func, 200);
        }
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
pub mod test {
    use super::*;
    use crate::message;
    use crate::dht::PeerRing;
    use crate::ecc::SecretKey;
    use crate::message::MessageHandler;
    use crate::session::SessionManager;
    use crate::swarm::Swarm;
    use crate::swarm::TransportManager;
    use crate::types::ice_transport::IceTrickleScheme;
    use std::sync;
    use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;

    use futures::lock::Mutex;
    use std::sync::Arc;

    pub async fn create_connected_pair(
        key1: SecretKey,
        key2: SecretKey,
    ) -> Result<(MessageHandler, MessageHandler)> {
        let stun = "stun://stun.l.google.com:19302";

        let dht1 = PeerRing::new(key1.address().into());
        let dht2 = PeerRing::new(key2.address().into());

        let session1 = SessionManager::new_with_seckey(&key1).unwrap();
        let session2 = SessionManager::new_with_seckey(&key2).unwrap();

        let swarm1 = Arc::new(Swarm::new(stun, key1.address(), session1.clone()));
        let swarm2 = Arc::new(Swarm::new(stun, key2.address(), session2.clone()));

        let transport1 = swarm1.new_transport().await.unwrap();
        let transport2 = swarm2.new_transport().await.unwrap();
        let handler1 = MessageHandler::new(Arc::new(Mutex::new(dht1)), Arc::clone(&swarm1));
        let handler2 = MessageHandler::new(Arc::new(Mutex::new(dht2)), Arc::clone(&swarm2));
        let handshake_info1 = transport1
            .get_handshake_info(session1.clone(), RTCSdpType::Offer)
            .await?;

        let addr1 = transport2.register_remote_info(handshake_info1).await?;

        let handshake_info2 = transport2
            .get_handshake_info(session2, RTCSdpType::Answer)
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
        assert!(handler1.listen_once().await.is_some());
        assert!(handler2.listen_once().await.is_some());
        Ok((handler1, handler2))
    }

    #[tokio::test]
    async fn test_custom_handler() -> Result<()> {
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();
        let addr1 = key1.address();
        let addr2 = key2.address();

        let (handler1, handler2) = create_connected_pair(key1, key2).await.unwrap();

        println!(
            "test with key1:{:?}, key2:{:?}",
            key1.address(),
            key2.address()
        );
        fn custom_handler(relay: &MessageRelay<Message>, id: Did) -> Result<()> {
            println!("{:?}, {:?}", relay, id);
            Ok(())
        }

        let scop_var: Arc<sync::Mutex<Vec<Did>>> = Arc::new(sync::Mutex::new(vec![]));

        let closure_handler = move |relay: &MessageRelay<Message>, id: Did| {
            let mut v = scop_var.lock().unwrap();
            v.push(id);
            println!("{:?}, {:?}", relay, id);
            Ok(())
        };

        let cb: CallbackFn = box custom_handler;
        let cb2: CallbackFn = box closure_handler;

        handler1.set_callback(cb).await;
        handler2.set_callback(cb2).await;

        handler1
            .send_message_default(&addr2, Message::custom("Hello world"))
            .await
            .unwrap();

        assert!(handler2.listen_once().await.is_some());

        handler2
            .send_message_default(&addr1, Message::custom("Hello world"))
            .await
            .unwrap();

        assert!(handler1.listen_once().await.is_some());
        Ok(())
    }

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

        println!("test with key1: {:?}, key2: {:?}, key3: {:?}", key1.address(), key2.address(), key3.address());

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
            assert!(!dht1.lock().await.successor.list().contains(&key1.address().into()));
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
            assert!(!dht2.lock().await.successor.list().contains(&key2.address().into()));
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
            assert!(!dht3.lock().await.successor.list().contains(&key3.address().into()));
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
            assert!(!dht2.lock().await.successor.list().contains(&key2.address().into()));
            assert_eq!(x.for_fix, false);

        } else {
            assert!(false);
        }

        println!("=======================================================");
        println!("||  now we connect join node3 to node1 via DHT       ||");
        println!("=======================================================");

        // node1's successor is node2
        assert_eq!(node1.dht.lock().await.successor.max(), key2.address().into());
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
        println!("test with key1: {:?}, key2: {:?}, key3: {:?}", key1.address(), key2.address(), key3.address());

        assert_eq!(&ev3.addr, &key2.address());
        assert_eq!(&ev3.to_path.clone(), &vec![key3.address().into()], "to_path not match!");
        assert_eq!(&ev3.from_path.clone(), &vec![key1.address().into(), key2.address().into()]);
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
        assert_eq!(&ev2.from_path.clone(), &vec![key1.address().into(), key2.address().into()]);
        assert_eq!(&ev2.to_path.clone(), &vec![key3.address().into()]);
        Ok(())
    }
}
