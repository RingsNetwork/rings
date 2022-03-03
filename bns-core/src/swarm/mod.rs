#[cfg(not(feature = "wasm"))]
use crate::channels::default::AcChannel as Channel;
#[cfg(feature = "wasm")]
use crate::channels::wasm::CbChannel as Channel;
use crate::dht::chord::Chord;
use crate::dht::chord::ChordAction;
use crate::dht::chord::RemoteAction;
use crate::ecc::SecretKey;
use crate::msg::SignedMsg;
/// Swarm is transport management
///
use crate::storage::{MemStorage, Storage};
#[cfg(not(feature = "wasm"))]
use crate::transports::default::DefaultTransport as Transport;
#[cfg(feature = "wasm")]
use crate::transports::wasm::WasmTransport as Transport;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::Events;
use crate::types::ice_transport::IceTransport;
use anyhow::Result;
use futures::lock::Mutex;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use web3::types::Address;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum Message {
    CustomMessage(String),
    DHTMessage(RemoteAction),
}

pub struct Swarm {
    pub table: MemStorage<Address, Arc<Transport>>,
    pub signaler: Arc<Channel>,
    pub stun_server: String,
    pub dht: Arc<Mutex<Chord>>,
    pub key: SecretKey,
}

impl Swarm {
    pub fn new(ch: Arc<Channel>, stun: String, key: SecretKey) -> Self {
        Self {
            table: MemStorage::<Address, Arc<Transport>>::new(),
            signaler: Arc::clone(&ch),
            stun_server: stun,
            dht: Arc::new(Mutex::new(Chord::new(key.address().into()))),
            key,
        }
    }

    pub fn address(&self) -> Address {
        self.key.address()
    }

    pub async fn new_transport(&self) -> Result<Arc<Transport>> {
        let mut ice_transport = Transport::new(self.signaler());
        ice_transport.start(self.stun_server.clone()).await?;
        let trans = Arc::new(ice_transport);
        Ok(Arc::clone(&trans))
    }

    pub async fn register(&self, address: Address, trans: Arc<Transport>) -> Result<()> {
        let prev_trans = self.table.set(address, trans);
        let mut dht = self.dht.lock().await;
        if let ChordAction::RemoteAction((addr, act)) = (*dht).join(address.into()) {
            let msg = SignedMsg::new(Message::DHTMessage(act), &self.key, None)?;
            // should handle return
            self.send_message_without_dht(addr.into(), msg)
                .await
                .unwrap();
        }
        if let Some(trans) = prev_trans {
            if let Err(e) = trans.close().await {
                log::error!("failed to close previous while registering {:?}", e);
            }
        }
        Ok(())
    }

    pub fn get_transport(&self, address: Address) -> Option<Arc<Transport>> {
        self.table.get(address)
    }

    pub fn signaler(&self) -> Arc<Channel> {
        Arc::clone(&self.signaler)
    }

    pub async fn send_message_without_dht(
        &self,
        address: Address,
        msg: SignedMsg<Message>,
    ) -> Result<()> {
        match self.get_transport(address) {
            Some(trans) => Ok(trans.send_message(msg).await?),
            None => Err(anyhow::anyhow!("cannot seek address in swarm table")),
        }
    }

    pub async fn event_handler(&self) {
        loop {
            match self.signaler.recv().await {
                Ok(ev) => match ev {
                    Events::ReceiveMsg(msg) => {
                        match serde_json::from_slice::<SignedMsg<Message>>(&msg) {
                            Ok(m) => {
                                if m.is_expired() || !m.verify() {
                                    log::error!("cannot verify msg or it's expired: {:?}", m);
                                } else {
                                    let _ = self.message_handler(m.to_owned().data).await;
                                }
                            }
                            Err(_e) => {
                                log::error!("cant handle Msg {:?}", msg);
                            }
                        }
                    }
                    x => {
                        log::debug!("Receive {:?}", x)
                    }
                },
                Err(e) => {
                    log::error!("failed on handle event {:?}", e)
                }
            }
        }
    }

    pub async fn message_handler(&self, msg: Message) {
        match msg {
            Message::CustomMessage(m) => {
                log::info!("got Msg {:?}", m);
            }
            Message::DHTMessage(action) => match action {
                RemoteAction::FindSuccessor(_) => {}
                RemoteAction::Notify(_) => {}
                RemoteAction::FindSuccessorAndAddToFinger((_, _)) => {}
                RemoteAction::CheckPredecessor => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

    fn new_swarm() -> Swarm {
        let ch = Arc::new(Channel::new(1));
        let stun = String::from("stun:stun.l.google.com:19302");
        Swarm::new(ch, stun, SecretKey::random())
    }

    #[tokio::test]
    async fn swarm_new_transport() {
        let swarm = new_swarm();

        let transport = swarm.new_transport().await.unwrap();
        assert_eq!(
            transport.ice_connection_state().await.unwrap(),
            RTCIceConnectionState::New
        );
    }

    #[tokio::test]
    async fn test_swarm_register_and_get() {
        let swarm1 = new_swarm();
        let swarm2 = new_swarm();

        assert!(swarm1.get_transport(swarm2.address()).is_none());

        let transport0 = swarm1.new_transport().await.unwrap();
        let transport1 = transport0.clone();

        swarm1.register(swarm2.address(), transport1).await.unwrap();

        let transport2 = swarm1.get_transport(swarm2.address()).unwrap();

        assert!(Arc::ptr_eq(&transport0, &transport2));
    }

    #[tokio::test]
    async fn test_swarm_will_close_previous_transport() {
        let swarm1 = new_swarm();
        let swarm2 = new_swarm();

        assert!(swarm1.get_transport(swarm2.address()).is_none());

        let transport1 = swarm1.new_transport().await.unwrap();
        let transport2 = swarm1.new_transport().await.unwrap();

        swarm1
            .register(swarm2.address(), transport1.clone())
            .await
            .unwrap();
        swarm1
            .register(swarm2.address(), transport2.clone())
            .await
            .unwrap();

        assert_eq!(
            transport1.ice_connection_state().await.unwrap(),
            RTCIceConnectionState::New
        );
        assert_eq!(
            transport1
                .get_peer_connection()
                .await
                .unwrap()
                .connection_state(),
            RTCPeerConnectionState::Closed
        );

        assert_eq!(
            transport2.ice_connection_state().await.unwrap(),
            RTCIceConnectionState::New
        );
        assert_eq!(
            transport2
                .get_peer_connection()
                .await
                .unwrap()
                .connection_state(),
            RTCPeerConnectionState::New
        );
    }

    #[tokio::test]
    async fn test_swarm_event_handler() {}
}
