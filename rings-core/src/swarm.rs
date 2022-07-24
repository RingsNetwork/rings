//! Tranposrt managerment
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

use async_stream::stream;
use async_trait::async_trait;
use futures::Stream;
use serde::de::DeserializeOwned;
use serde::Serialize;
use web3::types::Address;

use crate::channels::Channel;
use crate::err::Error;
use crate::err::Result;
use crate::message;
use crate::message::Decoder;
use crate::message::Encoder;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::session::SessionManager;
use crate::storage::MemStorage;
use crate::transports::Transport;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::Event;
use crate::types::ice_transport::IceServer;
use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportCallback;

pub struct Swarm {
    table: MemStorage<Address, Arc<Transport>>,
    pending: Arc<Mutex<Vec<Arc<Transport>>>>,
    ice_servers: Vec<IceServer>,
    transport_event_channel: Channel<Event>,
    session_manager: SessionManager,
    address: Address,
    external_address: Option<String>,
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
    async fn get_and_check_transport(&self, address: &Address) -> Option<Self::Transport>;
    async fn new_transport(&self) -> Result<Self::Transport>;
    async fn register(&self, address: &Address, trans: Self::Transport) -> Result<()>;
    async fn get_or_register(
        &self,
        address: &Address,
        default: Self::Transport,
    ) -> Result<Self::Transport>;
}

impl Swarm {
    pub fn new_with_external_address(
        ice_servers: &str,
        address: Address,
        session_manager: SessionManager,
        external_address: Option<String>,
    ) -> Self {
        let ice_servers = ice_servers
            .split(';')
            .collect::<Vec<&str>>()
            .into_iter()
            .map(|s| IceServer::from_str(s).unwrap())
            .collect::<Vec<IceServer>>();
        Self {
            table: MemStorage::<Address, Arc<Transport>>::new(),
            transport_event_channel: Channel::new(),
            ice_servers,
            address,
            session_manager,
            pending: Arc::new(Mutex::new(vec![])),
            external_address,
        }
    }

    pub fn new(ice_servers: &str, address: Address, session_manager: SessionManager) -> Self {
        Self::new_with_external_address(ice_servers, address, session_manager, None)
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub fn session_manager(&self) -> &SessionManager {
        &self.session_manager
    }

    fn load_message(&self, ev: Result<Option<Event>>) -> Result<Option<MessagePayload<Message>>> {
        let ev = ev?;

        match ev {
            Some(Event::DataChannelMessage(msg)) => {
                let payload = MessagePayload::from_encoded(&msg.try_into()?)?;
                Ok(Some(payload))
            }
            Some(Event::RegisterTransport(address)) => match self.get_transport(&address) {
                Some(_) => {
                    let payload = MessagePayload::new_direct(
                        Message::JoinDHT(message::JoinDHT { id: address.into() }),
                        &self.session_manager,
                        self.address().into(),
                    )?;
                    Ok(Some(payload))
                }
                None => Err(Error::SwarmMissTransport(address)),
            },
            Some(Event::ConnectClosed((address, uuid))) => {
                if let Ok(_) = self.pop_pending_transport(uuid) {
                    log::info!("Swarm: Pending transport {:?} dropped", uuid);
                };

                if self.remove_transport(&address).is_some() {
                    log::info!("Swarm: transport {:?} dropped", address);
                    let payload = MessagePayload::new_direct(
                        Message::LeaveDHT(message::LeaveDHT { id: address.into() }),
                        &self.session_manager,
                        self.address().into(),
                    )?;
                    Ok(Some(payload))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    /// This method is required because web-sys components is not `Send`
    /// which means an async loop cannot running concurrency.
    pub async fn poll_message(&self) -> Option<MessagePayload<Message>> {
        let receiver = &self.transport_event_channel.receiver();
        let ev = Channel::recv(receiver).await;
        match self.load_message(ev) {
            Ok(Some(msg)) => Some(msg),
            Ok(None) => None,
            Err(_) => None,
        }
    }

    pub fn iter_messages<'a, 'b>(&'a self) -> impl Stream<Item = MessagePayload<Message>> + 'b
    where 'a: 'b {
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
            .start(self.ice_servers.clone(), self.external_address.clone())
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

    async fn get_and_check_transport(&self, address: &Address) -> Option<Self::Transport> {
        match self.get_transport(address) {
            Some(t) => {
                if t.is_connected().await {
                    Some(t)
                } else {
                    if t.close().await.is_err() {
                        log::error!("Failed on close transport");
                    };
                    None
                }
            }
            None => None,
        }
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
        Ok(self.table.get_or_set(address, default))
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl<T> PayloadSender<T> for Swarm
where T: Clone + Serialize + DeserializeOwned + Send + Sync + 'static + fmt::Debug
{
    fn session_manager(&self) -> &SessionManager {
        Swarm::session_manager(self)
    }

    async fn do_send_payload(&self, address: &Address, payload: MessagePayload<T>) -> Result<()> {
        #[cfg(test)]
        {
            println!("+++++++++++++++++++++++++++++++++");
            println!("node {:?}", self.address());
            println!("Sent {:?}", payload.clone());
            println!("node {:?}", payload.relay.next_hop);
            println!("+++++++++++++++++++++++++++++++++");
        }
        log::trace!(
            "SENT {:?}, to node {:?}",
            payload.clone(),
            payload.relay.next_hop
        );
        let transport = self
            .get_and_check_transport(address)
            .await
            .ok_or(Error::SwarmMissAddressInTable)?;
        let data: Vec<u8> = payload.encode()?.into();
        transport.wait_for_data_channel_open().await?;
        transport.send_message(data.as_slice()).await
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod tests {
    use tokio::time;
    use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;

    use super::*;
    use crate::ecc::SecretKey;
    use crate::transports::default::transport::tests::establish_connection;

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
}
