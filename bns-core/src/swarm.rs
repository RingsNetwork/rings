/// Swarm is transport management
use crate::ecc::SecretKey;
use crate::message::Message;
use crate::message::MessageRelay;
use crate::storage::{MemStorage, Storage};
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::Event;
use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportCallback;
use anyhow::anyhow;
use anyhow::Result;
use async_stream::stream;
use futures_core::Stream;
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

pub struct Swarm {
    pub table: MemStorage<Address, Arc<Transport>>,
    pub signaler: Arc<Channel>,
    pub stun_server: String,
    pub key: SecretKey,
}

impl Swarm {
    pub fn new(ch: Arc<Channel>, stun: &str, key: SecretKey) -> Self {
        Self {
            table: MemStorage::<Address, Arc<Transport>>::new(),
            signaler: Arc::clone(&ch),
            stun_server: stun.into(),
            key,
        }
    }

    pub fn address(&self) -> Address {
        self.key.address()
    }

    pub async fn new_transport(&self) -> Result<Arc<Transport>> {
        let mut ice_transport = Transport::new(self.signaler());
        ice_transport
            .start(self.stun_server.as_str())
            .await?
            .apply_callback()
            .await?;
        Ok(Arc::new(ice_transport))
    }

    pub async fn register(&self, address: &Address, trans: Arc<Transport>) -> Result<()> {
        if !trans.is_connected().await {
            return Err(anyhow!("transport is not connected"));
        }

        let prev_trans = self.table.set(address, trans);
        if let Some(trans) = prev_trans {
            if let Err(e) = trans.close().await {
                log::error!("failed to close previous while registering {:?}", e);
            }
        }

        Ok(())
    }

    pub fn get_transport(&self, address: &Address) -> Option<Arc<Transport>> {
        self.table.get(address)
    }

    pub async fn get_or_register(
        &self,
        address: &Address,
        default: Arc<Transport>,
    ) -> Result<Arc<Transport>> {
        if !default.is_connected().await {
            return Err(anyhow!("default transport is not connected"));
        }

        Ok(self.table.get_or_set(address, default))
    }

    pub fn signaler(&self) -> Arc<Channel> {
        Arc::clone(&self.signaler)
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

    fn load_message(ev: Result<Event>) -> Result<MessageRelay<Message>> {
        // TODO: How to deal with events that is not message? Use mpmc?

        let ev = ev?;

        match ev {
            Event::ReceiveMsg(msg) => {
                let payload = serde_json::from_slice::<MessageRelay<Message>>(&msg)?;
                Ok(payload)
            }
            x => Err(anyhow!(format!("Receive {:?}", x))),
        }
    }

    pub fn iter_messages<'a, 'b>(&'a self) -> impl Stream<Item = MessageRelay<Message>> + 'b
    where
        'a: 'b,
    {
        stream! {
            loop {
                let ev = self.signaler().recv().await;
                if let Ok(msg) = Self::load_message(ev) {
                    yield msg
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transports::default::transport::tests::establish_connection;
    use tokio::time;
    use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;

    fn new_swarm() -> Swarm {
        let ch = Arc::new(Channel::new(1));
        let stun = "stun:stun.l.google.com:19302";
        let key = SecretKey::random();
        Swarm::new(ch, stun, key)
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
        assert!(swarm1
            .register(&swarm2.address(), transport1.clone())
            .await
            .is_err());
        assert!(swarm2
            .register(&swarm1.address(), transport2.clone())
            .await
            .is_err());

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
