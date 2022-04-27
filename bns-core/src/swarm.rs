use crate::err::{Error, Result};
use crate::message::{self, Message, MessageRelay, MessageRelayMethod};
use crate::session::SessionManager;
use crate::storage::{MemStorage, Storage};
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::Event;
use crate::types::ice_transport::IceServer;
use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportCallback;
use async_stream::stream;
use async_trait::async_trait;
use futures_core::Stream;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use web3::types::Address;

use crate::channels::Channel;
use crate::transports::Transport;

pub struct Swarm {
    table: MemStorage<Address, Arc<Transport>>,
    pending: Arc<Mutex<Vec<Arc<Transport>>>>,
    ice_servers: Vec<IceServer>,
    transport_event_channel: Channel<Event>,
    session: SessionManager,
    address: Address,
}

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

impl Swarm {
    pub fn new(ice_servers: &str, address: Address, session: SessionManager) -> Self {
        let ice_servers = ice_servers
            .split(';')
            .collect::<Vec<&str>>()
            .into_iter()
            .map(|_s| IceServer::from_str(_s).unwrap())
            .collect::<Vec<IceServer>>();
        Self {
            table: MemStorage::<Address, Arc<Transport>>::new(),
            transport_event_channel: Channel::new(1),
            ice_servers,
            address,
            session,
            pending: Arc::new(Mutex::new(vec![])),
        }
    }

    pub fn session(&self) -> SessionManager {
        self.session.clone()
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub async fn send_message(
        &self,
        address: &Address,
        payload: MessageRelay<Message>,
    ) -> Result<()> {
        let transport = self
            .get_transport(address)
            .ok_or(Error::SwarmMissAddressInTable)?;
        transport.wait_for_data_channel_open().await?;
        transport.send_message(&payload.gzip(9)?).await
    }

    fn load_message(&self, ev: Result<Option<Event>>) -> Result<Option<MessageRelay<Message>>> {
        let ev = ev?;

        match ev {
            Some(Event::DataChannelMessage(msg)) => {
                let payload = MessageRelay::from_gzipped(&msg)?;
                Ok(Some(payload))
            }
            Some(Event::RegisterTransport(address)) => match self.get_transport(&address) {
                Some(_) => {
                    let payload = MessageRelay::new(
                        Message::JoinDHT(message::JoinDHT { id: address.into() }),
                        &self.session,
                        None,
                        None,
                        None,
                        MessageRelayMethod::None,
                    )?;
                    Ok(Some(payload))
                }
                None => Err(Error::SwarmMissTransport(address)),
            },
            None => Ok(None),
            x => Err(Error::SwarmLoadMessageRecvFailed(format!("{:?}", x))),
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

    pub fn push_pending_transport(&self, transport: &Arc<Transport>) -> Result<()> {
        let mut pending = self
            .pending
            .try_lock()
            .map_err(|_| Error::SwarmPendingTransTryLockFailed)?;
        pending.push(transport.to_owned());
        Ok(())
    }

    pub fn pop_pending_transport(&self, transport_id: uuid::Uuid) -> Result<()> {
        let mut pending = self
            .pending
            .try_lock()
            .map_err(|_| Error::SwarmPendingTransTryLockFailed)?;
        let index = pending
            .iter()
            .position(|x| x.id.eq(&transport_id))
            .ok_or(Error::SwarmPendingTransNotFound)?;
        pending.remove(index);
        Ok(())
    }

    pub async fn pending_transports(&self) -> Result<Vec<Arc<Transport>>> {
        let pending = self
            .pending
            .try_lock()
            .map_err(|_| Error::SwarmPendingTransTryLockFailed)?;
        Ok(pending.iter().cloned().collect::<Vec<_>>())
    }

    pub fn find_pending_transport(&self, id: uuid::Uuid) -> Result<Option<Arc<Transport>>> {
        let pending = self
            .pending
            .try_lock()
            .map_err(|_| Error::SwarmPendingTransTryLockFailed)?;
        Ok(pending.iter().find(|x| x.id.eq(&id)).cloned())
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
            .start(&self.ice_servers[0])
            .await?
            .apply_callback()
            .await?;

        Ok(Arc::new(ice_transport))
    }

    /// register to swarm table
    /// should not wait connection statues here
    /// a connection `Promise` may cause deadlock of both end
    async fn register(&self, address: &Address, trans: Self::Transport) -> Result<()> {
        let prev_transport = self.table.set(address, trans);
        if let Some(transport) = prev_transport {
            if let Err(e) = transport.close().await {
                log::error!("failed to close previous while registering {:?}", e);
                return Err(Error::SwarmToClosePrevTransport(format!("{:?}", e)));
            }
        }

        Ok(())
    }

    fn get_transport(&self, address: &Address) -> Option<Self::Transport> {
        self.table.get(address)
    }

    fn remove_transport(&self, address: &Address) -> Option<(Address, Self::Transport)> {
        self.table.remove(address)
    }

    fn get_transport_numbers(&self) -> usize {
        self.table.len()
    }

    fn get_addresses(&self) -> Vec<Address> {
        self.table.keys()
    }

    fn get_transports(&self) -> Vec<(Address, Self::Transport)> {
        self.table.items()
    }

    async fn get_or_register(
        &self,
        address: &Address,
        default: Self::Transport,
    ) -> Result<Self::Transport> {
        if !default.is_connected().await {
            return Err(Error::SwarmDefaultTransportNotConnected);
        }
        Ok(self.table.get_or_set(address, default))
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;
    use crate::transports::default::transport::tests::establish_connection;
    use tokio::time;
    use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;

    fn new_swarm() -> Swarm {
        let stun = "stun://stun.l.google.com:19302";
        let key = SecretKey::random();
        let session = SessionManager::new_with_seckey(&key).unwrap();
        Swarm::new(stun, key.address(), session)
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
