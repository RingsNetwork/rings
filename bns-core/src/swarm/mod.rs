#[cfg(not(feature = "wasm"))]
use crate::channels::default::AcChannel as Channel;
#[cfg(feature = "wasm")]
use crate::channels::wasm::CbChannel as Channel;
use crate::dht::chord::Chord;
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
use crate::msg::SignedMsg;
use crate::ecc::SecretKey;
use anyhow::Result;
use std::sync::Arc;
use web3::types::Address;
use serde::Deserialize;
use serde::Serialize;


#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    CustomMessage { value: String },
    DHTMessage
}

pub struct Swarm {
    pub table: MemStorage<Address, Arc<Transport>>,
    pub signaler: Arc<Channel>,
    pub stun_server: String,
    pub dht: Chord,
    pub key: SecretKey,
}

impl Swarm {
    pub fn new(ch: Arc<Channel>, stun: String, key: SecretKey) -> Self {
        Self {
            table: MemStorage::<Address, Arc<Transport>>::new(),
            signaler: Arc::clone(&ch),
            stun_server: stun,
            dht: Chord::new(key.address().into()),
            key,
        }
    }

    pub async fn new_transport(&self) -> Result<Arc<Transport>> {
        let mut ice_transport = Transport::new(self.signaler());
        ice_transport.start(self.stun_server.clone()).await?;
        let trans = Arc::new(ice_transport);
        Ok(Arc::clone(&trans))
    }

    pub async fn register(&self, address: Address, trans: Arc<Transport>) {
        let prev_trans = self.table.set(address, trans);

        if let Some(trans) = prev_trans {
            if let Err(e) = trans.close().await {
                log::error!("failed to close previous while registering {:?}", e)
            }
        }
    }

    pub fn get_transport(&self, address: Address) -> Option<Arc<Transport>> {
        self.table.get(address)
    }

    pub fn signaler(&self) -> Arc<Channel> {
        Arc::clone(&self.signaler)
    }

    pub fn send_message(&self, address: Address, msg: SignedMsg<Message>) -> Result<()> {
        Ok(())
    }

    pub async fn event_handler(&self) {
        loop {
            match self.signaler.recv().await {
                Ok(ev) => match ev {
                    Events::ReceiveMsg(m) => {
                        let m = String::from_utf8(m).unwrap();
                        log::debug!("Receive Msg {}", m);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

    fn new_swarm(addr: &str) -> Swarm {
        let ch = Arc::new(Channel::new(1));
        let stun = String::from("stun:stun.l.google.com:19302");
        let address = Address::from_str(addr).unwrap();

        Swarm::new(ch, stun, address)
    }

    #[tokio::test]
    async fn swarm_new_transport() {
        let swarm = new_swarm("0x1111111111111111111111111111111111111111");

        let transport = swarm.new_transport().await.unwrap();
        assert_eq!(
            transport.ice_connection_state().await.unwrap(),
            RTCIceConnectionState::New
        );
    }

    #[tokio::test]
    async fn test_swarm_register_and_get() {
        let swarm1 = new_swarm("0x1111111111111111111111111111111111111111");
        let swarm2 = new_swarm("0x2222222222222222222222222222222222222222");

        assert!(swarm1.get_transport(swarm2.address).is_none());

        let transport0 = swarm1.new_transport().await.unwrap();
        let transport1 = transport0.clone();

        swarm1.register(swarm2.address, transport1).await;

        let transport2 = swarm1.get_transport(swarm2.address).unwrap();

        assert!(Arc::ptr_eq(&transport0, &transport2));
    }

    #[tokio::test]
    async fn test_swarm_will_close_previous_transport() {
        let swarm1 = new_swarm("0x1111111111111111111111111111111111111111");
        let swarm2 = new_swarm("0x2222222222222222222222222222222222222222");

        assert!(swarm1.get_transport(swarm2.address).is_none());

        let transport1 = swarm1.new_transport().await.unwrap();
        let transport2 = swarm1.new_transport().await.unwrap();

        swarm1.register(swarm2.address, transport1.clone()).await;
        swarm1.register(swarm2.address, transport2.clone()).await;

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
