/// Swarm is transport management
use crate::ecc::SecretKey;
use crate::message::{self, Message, MessageRelay, MessageRelayMethod};
use crate::storage::{MemStorage, Storage};
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::Event;
use crate::types::ice_transport::IceServer;
use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportCallback;
use anyhow::anyhow;
use anyhow::Result;
use async_stream::stream;
use async_trait::async_trait;
use dashmap::DashMap;
use futures_core::Stream;
use std::str::FromStr;
use std::sync::Arc;
use web3::types::Address;

#[cfg(not(feature = "wasm"))]
use crate::channels::default::AcChannel as Channel;
#[cfg(feature = "wasm")]
use crate::channels::wasm::CbChannel as Channel;

#[cfg(not(feature = "wasm"))]
use crate::transports::default::DefaultTransport as Transport;
#[cfg(feature = "wasm")]
use crate::transports::wasm::WasmTransport as Transport;

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TransportManager {
    type Transport;
    fn get_transports(&self) -> Vec<(Address, Self::Transport)>;
    fn get_addresses(&self) -> Vec<Address>;
    fn get_transport(&self, address: &Address) -> Option<Self::Transport>;
    fn remove_transport(&self, address: &Address) -> Option<(Address, Self::Transport)>;
    fn get_transport_numbers(&self) -> usize;
    async fn new_transport(&self) -> Result<Self::Transport>;
    async fn register(&self, address: &Address, trans: Self::Transport) -> Result<()>;
    async fn get_or_register(
        &self,
        address: &Address,
        default: Self::Transport,
    ) -> Result<Self::Transport>;
}

pub struct Swarm {
    transports: MemStorage<Address, Arc<Transport>>,
    descriptions: DashMap<String, Address>,
    transport_event_channel: Channel<Event>,
    ice_server: String,
    pub key: SecretKey,
}

impl Swarm {
    pub fn new(ice_server: &str, key: SecretKey) -> Self {
        Self {
            transports: MemStorage::<Address, Arc<Transport>>::new(),
            descriptions: DashMap::new(),
            transport_event_channel: Channel::new(1),
            ice_server: ice_server.into(),
            key,
        }
    }

    pub fn set_local_description(&self, description: String, address: Address) {
        self.descriptions.insert(description, address);
    }

    pub fn get_local_address(&self, description: &str) -> Option<Address> {
        self.descriptions.get(description).map(|v| *v.value())
    }

    pub fn remove_local_address(&self, description: &str) -> Option<(String, Address)> {
        match self.get_local_address(description) {
            Some(_) => self.descriptions.remove(description),
            None => None,
        }
    }

    pub fn address(&self) -> Address {
        self.key.address()
    }

    pub async fn send_message(
        &self,
        address: &Address,
        payload: MessageRelay<Message>,
    ) -> Result<()> {
        match self.get_transport(address) {
            Some(trans) => Ok(trans.send_message(payload).await?),
            None => Err(anyhow!("cannot seek address in swarm table")),
        }
    }

    fn load_message(&self, ev: Result<Option<Event>>) -> Result<Option<MessageRelay<Message>>> {
        // TODO: How to deal with events that is not message? Use mpmc?

        let ev = ev?;

        match ev {
            Some(Event::DataChannelMessage(msg)) => {
                let payload = serde_json::from_slice::<MessageRelay<Message>>(&msg)?;
                Ok(Some(payload))
            }
            Some(Event::RegisterTransport(sdp)) => {
                let address = self.get_local_address(&sdp).unwrap();
                match self.get_transport(&address) {
                    Some(_) => {
                        let payload = MessageRelay::new(
                            Message::JoinDHT(message::JoinDHT { id: address.into() }),
                            &self.key,
                            None,
                            None,
                            None,
                            MessageRelayMethod::None,
                        )?;
                        Ok(Some(payload))
                    }
                    None => Err(anyhow!(format!(
                        "Cannot get transport from address {:?}",
                        address
                    ))),
                }
            }
            None => Ok(None),
            x => Err(anyhow!(format!("Receive {:?}", x))),
        }
    }

    /// This method is required because web-sys components is not `Send`
    /// which means an async loop cannot running concurrency.
    pub async fn poll_message(&self) -> Option<MessageRelay<Message>> {
        let receiver = &self.transport_event_channel.receiver();
        let ev = Channel::recv(receiver).await;
        match self.load_message(ev) {
            Ok(Some(msg)) => Some(msg),
            Ok(None) => None,
            Err(_) => None,
        }
    }

    pub fn iter_messages<'a, 'b>(&'a self) -> impl Stream<Item = MessageRelay<Message>> + 'b
    where
        'a: 'b,
    {
        stream! {
            let receiver = &self.transport_event_channel.receiver();
            loop {
                let ev = Channel::recv(receiver).await;
                if let Ok(Some(msg)) = self.load_message(ev) {
                    yield msg
                }
            }
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl TransportManager for Swarm {
    type Transport = Arc<Transport>;

    async fn new_transport(&self) -> Result<Self::Transport> {
        let event_sender = self.transport_event_channel.sender();
        let mut ice_transport = Transport::new(event_sender);

        ice_transport
            .start(&IceServer::from_str(&self.ice_server)?)
            .await?
            .apply_callback()
            .await?;

        Ok(Arc::new(ice_transport))
    }

    /// register to swarm table
    /// should not wait connection statues here
    /// a connection `Promise` may cause deadlock of both end
    async fn register(&self, address: &Address, trans: Self::Transport) -> Result<()> {
        let prev_trans = self.transports.set(address, trans);
        if let Some(trans) = prev_trans {
            if let Err(e) = trans.close().await {
                log::error!("failed to close previous while registering {:?}", e);
            }
        }

        Ok(())
    }

    fn get_transport(&self, address: &Address) -> Option<Self::Transport> {
        self.transports.get(address)
    }

    fn remove_transport(&self, address: &Address) -> Option<(Address, Self::Transport)> {
        self.transports.remove(address)
    }

    fn get_transport_numbers(&self) -> usize {
        self.transports.len()
    }

    fn get_addresses(&self) -> Vec<Address> {
        self.transports.keys()
    }

    fn get_transports(&self) -> Vec<(Address, Self::Transport)> {
        self.transports.items()
    }

    async fn get_or_register(
        &self,
        address: &Address,
        default: Self::Transport,
    ) -> Result<Self::Transport> {
        if !default.is_connected().await {
            return Err(anyhow!("default transport is not connected"));
        }

        Ok(self.transports.get_or_set(address, default))
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transports::default::transport::tests::establish_connection;
    use tokio::time;
    use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;

    fn new_swarm() -> Swarm {
        let stun = "stun://stun.l.google.com:19302";
        let key = SecretKey::random();
        Swarm::new(stun, key)
    }

    #[tokio::test]
    async fn swarm_new_transport() -> Result<()> {
        let swarm = new_swarm();

        let transport = swarm.new_transport().await.unwrap();
        assert_eq!(
            transport.ice_connection_state().await.unwrap(),
            RTCIceConnectionState::New
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_swarm_register_and_get() -> Result<()> {
        let swarm1 = new_swarm();
        let swarm2 = new_swarm();

        assert!(swarm1.get_transport(&swarm2.address()).is_none());
        assert!(swarm2.get_transport(&swarm1.address()).is_none());

        let transport1 = swarm1.new_transport().await.unwrap();
        let transport2 = swarm2.new_transport().await.unwrap();

        // Cannot register if not connected
        // assert!(swarm1
        //     .register(&swarm2.address(), transport1.clone())
        //     .await
        //     .is_err());
        // assert!(swarm2
        //     .register(&swarm1.address(), transport2.clone())
        //     .await
        //     .is_err());

        establish_connection(&transport1, &transport2).await?;

        // Can register if connected
        swarm1
            .register(&swarm2.address(), transport1.clone())
            .await?;
        swarm2
            .register(&swarm1.address(), transport2.clone())
            .await?;

        // Check address transport pairs in table
        let transport_1_to_2 = swarm1.get_transport(&swarm2.address()).unwrap();
        let transport_2_to_1 = swarm2.get_transport(&swarm1.address()).unwrap();

        assert!(Arc::ptr_eq(&transport_1_to_2, &transport1));
        assert!(Arc::ptr_eq(&transport_2_to_1, &transport2));

        Ok(())
    }

    #[tokio::test]
    async fn test_swarm_will_close_previous_transport() -> Result<()> {
        let swarm1 = new_swarm();
        let swarm2 = new_swarm();

        assert!(swarm1.get_transport(&swarm2.address()).is_none());

        let transport0 = swarm1.new_transport().await.unwrap();
        let transport1 = swarm1.new_transport().await.unwrap();

        let transport_2_to_0 = swarm2.new_transport().await.unwrap();
        let transport_2_to_1 = swarm2.new_transport().await.unwrap();

        establish_connection(&transport0, &transport_2_to_0).await?;
        establish_connection(&transport1, &transport_2_to_1).await?;

        swarm1
            .register(&swarm2.address(), transport0.clone())
            .await?;
        swarm1
            .register(&swarm2.address(), transport1.clone())
            .await?;

        time::sleep(time::Duration::from_secs(3)).await;

        assert_eq!(
            transport0.ice_connection_state().await.unwrap(),
            RTCIceConnectionState::Closed
        );
        assert_eq!(
            transport_2_to_0.ice_connection_state().await.unwrap(),
            RTCIceConnectionState::Connected
        );
        // TODO: Find a way to maintain transports in another peer.

        assert_eq!(
            transport1.ice_connection_state().await.unwrap(),
            RTCIceConnectionState::Connected
        );
        assert_eq!(
            transport_2_to_1.ice_connection_state().await.unwrap(),
            RTCIceConnectionState::Connected
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_swarm_event_handler() -> Result<()> {
        Ok(())
    }
}
