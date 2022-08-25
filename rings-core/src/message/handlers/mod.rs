use std::sync::Arc;

use async_recursion::async_recursion;
use async_trait::async_trait;
use futures::lock::Mutex;
use web3::types::Address;

use super::CustomMessage;
use super::MaybeEncrypted;
use super::Message;
use super::MessagePayload;
use super::OriginVerificationGen;
use super::PayloadSender;
use crate::dht::Chord;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::err::Error;
use crate::err::Result;
use crate::prelude::RTCSdpType;
use crate::prelude::Transport;
use crate::session::SessionManager;
use crate::swarm::Swarm;
use crate::swarm::TransportManager;
use crate::types::ice_transport::IceTransportInterface;
use crate::types::ice_transport::IceTrickleScheme;

/// Operator and Handler for Connection
pub mod connection;
/// Operator and Handler for CustomMessage
pub mod custom;
/// Operator and handler for DHT stablization
pub mod stabilization;
/// Operator and Handler for Storage
pub mod storage;
/// Operator and Handler for SubRing
pub mod subring;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait MessageCallback {
    async fn custom_message(
        &self,
        handler: &MessageHandler,
        ctx: &MessagePayload<Message>,
        msg: &MaybeEncrypted<CustomMessage>,
    );
    async fn builtin_message(&self, handler: &MessageHandler, ctx: &MessagePayload<Message>);
}

#[cfg(not(feature = "wasm"))]
type CallbackFn = Box<dyn MessageCallback + Send + Sync>;

#[cfg(feature = "wasm")]
type CallbackFn = Box<dyn MessageCallback>;

#[derive(Clone)]
pub struct MessageHandler {
    dht: Arc<PeerRing>,
    swarm: Arc<Swarm>,
    callback: Arc<Mutex<Option<CallbackFn>>>,
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait HandleMsg<T> {
    async fn handle(&self, ctx: &MessagePayload<Message>, msg: &T) -> Result<()>;
}

impl MessageHandler {
    pub fn new_with_callback(dht: Arc<PeerRing>, swarm: Arc<Swarm>, callback: CallbackFn) -> Self {
        Self {
            dht,
            swarm,
            callback: Arc::new(Mutex::new(Some(callback))),
        }
    }

    pub fn new(dht: Arc<PeerRing>, swarm: Arc<Swarm>) -> Self {
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

    // disconnect a node if a node is in DHT
    pub async fn disconnect(&self, address: Address) -> Result<()> {
        log::info!("disconnect {:?}", address);
        self.dht.remove(address.into())?;
        if let Some((_address, trans)) = self.swarm.remove_transport(&address) {
            trans.close().await?
        }
        Ok(())
    }

    pub async fn connect(&self, address: &Address) -> Result<Arc<Transport>> {
        if let Some(t) = self.swarm.get_and_check_transport(address).await {
            return Ok(t);
        }

        let target_id = address.to_owned().into();
        let transport = self.swarm.new_transport().await?;
        let handshake_info = transport
            .get_handshake_info(self.swarm.session_manager(), RTCSdpType::Offer)
            .await?;
        self.swarm.push_pending_transport(&transport)?;

        let connect_msg = Message::ConnectNodeSend(super::ConnectNodeSend {
            transport_uuid: transport.id.to_string(),
            handshake_info: handshake_info.to_string(),
        });
        let next_hop = {
            match self.dht.find_successor(target_id)? {
                PeerRingAction::Some(node) => Some(node),
                PeerRingAction::RemoteAction(node, _) => Some(node),
                _ => None,
            }
        }
        .ok_or(Error::NoNextHop)?;
        log::debug!("next_hop: {:?}", next_hop);
        self.send_message(connect_msg, next_hop, target_id).await?;
        Ok(transport)
    }

    async fn invoke_callback(&self, payload: &MessagePayload<Message>) -> Result<()> {
        let mut callback = self.callback.lock().await;
        if let Some(ref mut cb) = *callback {
            match payload.data {
                Message::CustomMessage(ref msg) => {
                    if self.dht.id == payload.relay.destination {
                        cb.custom_message(self, payload, msg).await
                    }
                }
                _ => cb.builtin_message(self, payload).await,
            };
        }
        Ok(())
    }

    pub fn decrypt_msg(&self, msg: &MaybeEncrypted<CustomMessage>) -> Result<CustomMessage> {
        let key = self.swarm.session_manager().session_key()?;
        let (decrypt_msg, _) = msg.to_owned().decrypt(&key)?;
        Ok(decrypt_msg)
    }

    #[cfg_attr(feature = "wasm", async_recursion(?Send))]
    #[cfg_attr(not(feature = "wasm"), async_recursion)]
    pub async fn handle_payload(&self, payload: &MessagePayload<Message>) -> Result<()> {
        #[cfg(test)]
        {
            println!("{} got msg {}", self.swarm.address(), &payload.data);
        }
        log::trace!("NEW MESSAGE: {}", &payload.data);
        match &payload.data {
            Message::JoinDHT(ref msg) => self.handle(payload, msg).await,
            Message::LeaveDHT(ref msg) => self.handle(payload, msg).await,
            Message::ConnectNodeSend(ref msg) => self.handle(payload, msg).await,
            Message::ConnectNodeReport(ref msg) => self.handle(payload, msg).await,
            Message::AlreadyConnected(ref msg) => self.handle(payload, msg).await,
            Message::FindSuccessorSend(ref msg) => self.handle(payload, msg).await,
            Message::FindSuccessorReport(ref msg) => self.handle(payload, msg).await,
            Message::NotifyPredecessorSend(ref msg) => self.handle(payload, msg).await,
            Message::NotifyPredecessorReport(ref msg) => self.handle(payload, msg).await,
            Message::SearchVNode(ref msg) => self.handle(payload, msg).await,
            Message::FoundVNode(ref msg) => self.handle(payload, msg).await,
            Message::StoreVNode(ref msg) => self.handle(payload, msg).await,
            Message::CustomMessage(ref msg) => self.handle(payload, msg).await,
            Message::MultiCall(ref msg) => {
                for message in msg.messages.iter().cloned() {
                    let payload = MessagePayload::new(
                        message,
                        self.swarm.session_manager(),
                        OriginVerificationGen::Stick(payload.origin_verification.clone()),
                        payload.relay.clone(),
                    )?;
                    self.handle_payload(&payload).await.unwrap_or(());
                }
                Ok(())
            }
            x => Err(Error::MessageHandlerUnsupportMessageType(format!(
                "{:?}",
                x
            ))),
        }?;
        if let Err(e) = self.invoke_callback(payload).await {
            log::warn!("invoke callback error: {}", e);
        }

        Ok(())
    }

    /// This method is required because web-sys components is not `Send`
    /// which means a listening loop cannot running concurrency.
    pub async fn listen_once(&self) -> Option<MessagePayload<Message>> {
        if let Some(payload) = self.swarm.poll_message().await {
            if !payload.verify() {
                log::error!("Cannot verify msg or it's expired: {:?}", payload);
            }
            if let Err(e) = self.handle_payload(&payload).await {
                log::error!("Error in handle_message: {}", e);
                #[cfg(test)]
                {
                    println!("Error in handle_message: {}", e);
                }
            }
            Some(payload)
        } else {
            None
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl PayloadSender<Message> for MessageHandler {
    fn session_manager(&self) -> &SessionManager {
        self.swarm.session_manager()
    }

    async fn do_send_payload(
        &self,
        address: &Address,
        payload: MessagePayload<Message>,
    ) -> Result<()> {
        self.swarm.do_send_payload(address, payload).await
    }
}

#[cfg(not(feature = "wasm"))]
mod listener {
    use std::sync::Arc;

    use async_trait::async_trait;
    use futures::pin_mut;
    use futures::stream::StreamExt;

    use super::MessageHandler;
    use crate::types::message::MessageListener;

    #[async_trait]
    impl MessageListener for MessageHandler {
        async fn listen(self: Arc<Self>) {
            let payloads = self.swarm.iter_messages().await;
            pin_mut!(payloads);
            while let Some(payload) = payloads.next().await {
                if !payload.verify() {
                    log::error!("Cannot verify msg or it's expired: {:?}", payload);
                    continue;
                }
                if let Err(e) = self.handle_payload(&payload).await {
                    log::error!("Error in handle_message: {}", e);
                    continue;
                }
            }
        }
    }
}

#[cfg(feature = "wasm")]
mod listener {
    use std::sync::Arc;

    use async_trait::async_trait;
    use wasm_bindgen_futures::spawn_local;

    use super::MessageHandler;
    use crate::poll;
    use crate::types::message::MessageListener;

    #[async_trait(?Send)]
    impl MessageListener for MessageHandler {
        async fn listen(self: Arc<Self>) {
            let handler = Arc::clone(&self);
            let func = move || {
                let handler = handler.clone();
                spawn_local(Box::pin(async move {
                    handler.listen_once().await;
                }));
            };
            poll!(func, 1000);
        }
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
pub mod tests {
    use std::sync::Arc;

    use futures::lock::Mutex;
    use tokio::time::sleep;
    use tokio::time::Duration;
    use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;

    use super::*;
    use crate::dht::Did;
    use crate::dht::PeerRing;
    use crate::ecc::SecretKey;
    use crate::message::MessageHandler;
    use crate::session::SessionManager;
    use crate::storage::PersistenceStorage;
    use crate::swarm::Swarm;
    use crate::swarm::TransportManager;
    use crate::types::ice_transport::IceTrickleScheme;
    use crate::types::message::MessageListener;

    pub async fn prepare_node(
        key: &SecretKey,
    ) -> (Did, Arc<PeerRing>, Arc<Swarm>, MessageHandler, String) {
        let stun = "stun://stun.l.google.com:19302";

        let did = key.address().into();
        let path = PersistenceStorage::random_path("./tmp");
        let dht = Arc::new(PeerRing::new_with_storage(
            did,
            Arc::new(
                PersistenceStorage::new_with_path(path.as_str())
                    .await
                    .unwrap(),
            ),
        ));
        let sm = SessionManager::new_with_seckey(key).unwrap();
        let swarm = Arc::new(Swarm::new(stun, key.address(), sm));
        let node = MessageHandler::new(dht.clone(), Arc::clone(&swarm));

        println!("key: {:?}", key.to_string());
        println!("did: {:?}", did);

        (did, dht, swarm, node, path)
    }

    pub async fn manually_establish_connection(swarm1: &Swarm, swarm2: &Swarm) -> Result<()> {
        let sm1 = swarm1.session_manager();
        let sm2 = swarm2.session_manager();

        let transport1 = swarm1.new_transport().await.unwrap();
        let handshake_info1 = transport1
            .get_handshake_info(sm1, RTCSdpType::Offer)
            .await?;

        let transport2 = swarm2.new_transport().await.unwrap();
        let addr1 = transport2.register_remote_info(handshake_info1).await?;

        assert_eq!(addr1, swarm1.address());

        let handshake_info2 = transport2
            .get_handshake_info(sm2, RTCSdpType::Answer)
            .await?;

        let addr2 = transport1.register_remote_info(handshake_info2).await?;

        assert_eq!(addr2, swarm2.address());

        let promise_1 = transport1.connect_success_promise().await?;
        let promise_2 = transport2.connect_success_promise().await?;
        promise_1.await?;
        promise_2.await?;

        swarm2
            .register(&swarm1.address(), transport2.clone())
            .await
            .unwrap();

        swarm1
            .register(&swarm2.address(), transport1.clone())
            .await
            .unwrap();

        assert!(swarm1.get_transport(&swarm2.address()).is_some());
        assert!(swarm2.get_transport(&swarm1.address()).is_some());

        Ok(())
    }

    #[derive(Clone)]
    struct MessageCallbackInstance {
        #[allow(clippy::type_complexity)]
        handler_messages: Arc<Mutex<Vec<(Did, Vec<u8>)>>>,
    }

    #[tokio::test]
    async fn test_custom_message_handling() -> Result<()> {
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();

        let (did1, _dht1, swarm1, handler1, _path1) = prepare_node(&key1).await;
        let (did2, _dht2, swarm2, handler2, _path2) = prepare_node(&key2).await;

        manually_establish_connection(&swarm1, &swarm2).await?;

        #[async_trait]
        impl MessageCallback for MessageCallbackInstance {
            async fn custom_message(
                &self,
                handler: &MessageHandler,
                ctx: &MessagePayload<Message>,
                msg: &MaybeEncrypted<CustomMessage>,
            ) {
                let decrypted_msg = handler.decrypt_msg(msg).unwrap();
                self.handler_messages
                    .lock()
                    .await
                    .push((ctx.addr.into(), decrypted_msg.0));
                println!("{:?}, {:?}, {:?}", ctx, ctx.addr, msg);
            }

            async fn builtin_message(
                &self,
                _handler: &MessageHandler,
                ctx: &MessagePayload<Message>,
            ) {
                println!("{:?}, {:?}", ctx, ctx.addr);
            }
        }

        let msg_callback1 = MessageCallbackInstance {
            handler_messages: Arc::new(Mutex::new(vec![])),
        };
        let msg_callback2 = MessageCallbackInstance {
            handler_messages: Arc::new(Mutex::new(vec![])),
        };
        let cb1: CallbackFn = Box::new(msg_callback1.clone());
        let cb2: CallbackFn = Box::new(msg_callback2.clone());

        handler1.set_callback(cb1).await;
        handler2.set_callback(cb2).await;

        handler1
            .send_direct_message(
                Message::custom("Hello world 1 to 2 - 1".as_bytes(), &None)?,
                did2,
            )
            .await
            .unwrap();

        handler1
            .send_direct_message(
                Message::custom("Hello world 1 to 2 - 2".as_bytes(), &None)?,
                did2,
            )
            .await
            .unwrap();

        handler2
            .send_direct_message(
                Message::custom("Hello world 2 to 1 - 1".as_bytes(), &None)?,
                did1,
            )
            .await
            .unwrap();

        handler1
            .send_direct_message(
                Message::custom("Hello world 1 to 2 - 3".as_bytes(), &None)?,
                did2,
            )
            .await
            .unwrap();

        handler2
            .send_direct_message(
                Message::custom("Hello world 2 to 1 - 2".as_bytes(), &None)?,
                did1,
            )
            .await
            .unwrap();

        tokio::spawn(async { Arc::new(handler1).listen().await });
        tokio::spawn(async { Arc::new(handler2).listen().await });

        sleep(Duration::from_secs(5)).await;

        assert_eq!(msg_callback1.handler_messages.lock().await.as_slice(), &[
            (did2, "Hello world 2 to 1 - 1".as_bytes().to_vec()),
            (did2, "Hello world 2 to 1 - 2".as_bytes().to_vec())
        ]);

        assert_eq!(msg_callback2.handler_messages.lock().await.as_slice(), &[
            (did1, "Hello world 1 to 2 - 1".as_bytes().to_vec()),
            (did1, "Hello world 1 to 2 - 2".as_bytes().to_vec()),
            (did1, "Hello world 1 to 2 - 3".as_bytes().to_vec())
        ]);

        Ok(())
    }
}
