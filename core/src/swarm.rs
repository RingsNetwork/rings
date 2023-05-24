//! Tranposrt managerment
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::channels::Channel;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::ecc::SecretKey;
use crate::err::Error;
use crate::err::Result;
use crate::inspect::SwarmInspect;
use crate::measure::Measure;
use crate::measure::MeasureCounter;
use crate::message;
use crate::message::CallbackFn;
use crate::message::ChordStorageInterface;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessageHandlerEvent;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::message::ValidatorFn;
use crate::session::SessionManager;
use crate::session::Ttl;
use crate::storage::MemStorage;
use crate::storage::PersistenceStorage;
use crate::transports::manager::TransportHandshake;
use crate::transports::manager::TransportManager;
use crate::transports::Transport;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::TransportEvent;
use crate::types::ice_transport::IceServer;
use crate::types::ice_transport::IceTransportInterface;
use crate::types::ice_transport::IceTrickleScheme;

#[cfg(not(feature = "wasm"))]
pub type MeasureImpl = Box<dyn Measure + Send + Sync>;

#[cfg(feature = "wasm")]
pub type MeasureImpl = Box<dyn Measure>;

/// Creates a SwarmBuilder to configure a Swarm.
pub struct SwarmBuilder {
    key: Option<SecretKey>,
    ice_servers: Vec<IceServer>,
    external_address: Option<String>,
    dht_did: Option<Did>,
    dht_succ_max: u8,
    dht_storage: PersistenceStorage,
    session_manager: Option<SessionManager>,
    session_ttl: Option<Ttl>,
    measure: Option<MeasureImpl>,
    message_callback: Option<CallbackFn>,
    message_validator: Option<ValidatorFn>,
}

impl SwarmBuilder {
    pub fn new(ice_servers: &str, dht_storage: PersistenceStorage) -> Self {
        let ice_servers = ice_servers
            .split(';')
            .collect::<Vec<&str>>()
            .into_iter()
            .map(|s| IceServer::from_str(s).unwrap())
            .collect::<Vec<IceServer>>();
        SwarmBuilder {
            key: None,
            ice_servers,
            external_address: None,
            dht_did: None,
            dht_succ_max: 3,
            dht_storage,
            session_manager: None,
            session_ttl: None,
            measure: None,
            message_callback: None,
            message_validator: None,
        }
    }

    pub fn dht_succ_max(mut self, succ_max: u8) -> Self {
        self.dht_succ_max = succ_max;
        self
    }

    pub fn external_address(mut self, external_address: Option<String>) -> Self {
        self.external_address = external_address;
        self
    }

    pub fn key(mut self, key: SecretKey) -> Self {
        self.key = Some(key);
        self.dht_did = Some(key.address().into());
        self
    }

    pub fn session_manager(mut self, did: Did, session_manager: SessionManager) -> Self {
        self.session_manager = Some(session_manager);
        self.dht_did = Some(did);
        self
    }

    pub fn session_ttl(mut self, ttl: Ttl) -> Self {
        self.session_ttl = Some(ttl);
        self
    }

    pub fn measure(mut self, implement: MeasureImpl) -> Self {
        self.measure = Some(implement);
        self
    }

    pub fn message_callback(mut self, callback: Option<CallbackFn>) -> Self {
        self.message_callback = callback;
        self
    }

    pub fn message_validator(mut self, validator: ValidatorFn) -> Self {
        self.message_validator = Some(validator);
        self
    }

    pub fn build(self) -> Result<Swarm> {
        let session_manager = {
            if self.session_manager.is_some() {
                Ok(self.session_manager.unwrap())
            } else if self.key.is_some() {
                SessionManager::new_with_seckey(&self.key.unwrap(), self.session_ttl)
            } else {
                Err(Error::SwarmBuildFailed(
                    "Should set session_manager or key".into(),
                ))
            }
        }?;

        let dht_did = self
            .dht_did
            .ok_or_else(|| Error::SwarmBuildFailed("Should set session_manager or key".into()))?;

        let dht = Arc::new(PeerRing::new_with_storage(
            dht_did,
            self.dht_succ_max,
            self.dht_storage,
        ));

        let message_handler =
            MessageHandler::new(dht.clone(), self.message_callback, self.message_validator);

        Ok(Swarm {
            pending_transports: Mutex::new(vec![]),
            transports: MemStorage::new(),
            transport_event_channel: Channel::new(),
            ice_servers: self.ice_servers,
            external_address: self.external_address,
            dht,
            measure: self.measure,
            session_manager,
            message_handler,
        })
    }
}

/// The transports and dht management.
pub struct Swarm {
    pub(crate) pending_transports: Mutex<Vec<Arc<Transport>>>,
    pub(crate) transports: MemStorage<Did, Arc<Transport>>,
    pub(crate) ice_servers: Vec<IceServer>,
    pub(crate) transport_event_channel: Channel<TransportEvent>,
    pub(crate) external_address: Option<String>,
    pub(crate) dht: Arc<PeerRing>,
    pub(crate) measure: Option<MeasureImpl>,
    session_manager: SessionManager,
    message_handler: MessageHandler,
}

impl Swarm {
    pub fn did(&self) -> Did {
        self.dht.did
    }

    pub fn dht(&self) -> Arc<PeerRing> {
        self.dht.clone()
    }

    pub fn session_manager(&self) -> &SessionManager {
        &self.session_manager
    }

    async fn load_message(
        &self,
        ev: Result<Option<TransportEvent>>,
    ) -> Result<Option<MessagePayload<Message>>> {
        let ev = ev?;

        match ev {
            Some(TransportEvent::DataChannelMessage(msg)) => {
                let payload = MessagePayload::from_bincode(&msg)?;
                tracing::debug!("load message from channel: {:?}", payload);
                Ok(Some(payload))
            }
            Some(TransportEvent::RegisterTransport((did, id))) => {
                // if transport is still pending
                if let Ok(Some(t)) = self.find_pending_transport(id) {
                    tracing::debug!("transport is inside pending list, mov to swarm transports");

                    self.register(did, t).await?;
                    self.pop_pending_transport(id)?;
                }
                match self.get_transport(did) {
                    Some(_) => {
                        let payload = MessagePayload::new_send(
                            Message::JoinDHT(message::JoinDHT { did }),
                            &self.session_manager,
                            self.dht.did,
                            self.dht.did,
                        )?;
                        Ok(Some(payload))
                    }
                    None => Err(Error::SwarmMissTransport(did)),
                }
            }
            Some(TransportEvent::ConnectClosed((did, uuid))) => {
                if self.pop_pending_transport(uuid).is_ok() {
                    tracing::info!(
                        "[Swarm::ConnectClosed] Pending transport {:?} dropped",
                        uuid
                    );
                };

                if let Some(t) = self.get_transport(did) {
                    if t.id == uuid && self.remove_transport(did).is_some() {
                        tracing::info!("[Swarm::ConnectClosed] transport {:?} closed", uuid);
                        let payload = MessagePayload::new_send(
                            Message::LeaveDHT(message::LeaveDHT { did }),
                            &self.session_manager,
                            self.dht.did,
                            self.dht.did,
                        )?;
                        return Ok(Some(payload));
                    }
                }
                Ok(None)
            }
            None => Ok(None),
        }
    }

    /// This method is required because web-sys components is not `Send`
    /// which means an async loop cannot running concurrency.
    pub async fn poll_message(&self) -> Option<MessagePayload<Message>> {
        let receiver = &self.transport_event_channel.receiver();
        let ev = Channel::recv(receiver).await;
        match self.load_message(ev).await {
            Ok(Some(msg)) => Some(msg),
            Ok(None) => None,
            Err(_) => None,
        }
    }

    /// This method is required because web-sys components is not `Send`
    /// which means a listening loop cannot running concurrency.
    pub async fn listen_once(&self) -> Option<(MessagePayload<Message>, Vec<MessageHandlerEvent>)> {
        let payload = self.poll_message().await?;

        if !payload.verify() {
            tracing::error!("Cannot verify msg or it's expired: {:?}", payload);
            return None;
        }

        let mut events = self.message_handler.handle_message(&payload).await.ok()?;
        let mut extra_events = vec![];

        for ev in &events {
            let evs = self.handle_message_handler_event(&payload, ev).await.ok()?;

            for sub_ev in &evs {
                self.handle_message_handler_event(&payload, sub_ev)
                    .await
                    .ok()?;
            }

            extra_events.extend(evs);
        }

        events.extend(extra_events);
        Some((payload, events))
    }

    pub async fn handle_message_handler_event(
        &self,
        payload: &MessagePayload<Message>,
        event: &MessageHandlerEvent,
    ) -> Result<Vec<MessageHandlerEvent>> {
        tracing::debug!("Handle message handler event: {:?}", event);
        match event {
            MessageHandlerEvent::Connect(did) => {
                self.connect(*did).await?;
            }
            MessageHandlerEvent::Disconnect(did) => {
                self.disconnect(*did).await?;
            }
            MessageHandlerEvent::AnswerOffer(msg) => {
                let (_, answer) = self
                    .answer_remote_transport(payload.relay.sender(), msg)
                    .await?;

                return Ok(vec![MessageHandlerEvent::SendReportMessage(
                    Message::ConnectNodeReport(answer),
                )]);
            }

            MessageHandlerEvent::AcceptAnswer(msg) => {
                let transport = self
                    .find_pending_transport(
                        uuid::Uuid::from_str(&msg.transport_uuid)
                            .map_err(|_| Error::InvalidTransportUuid)?,
                    )?
                    .ok_or(Error::MessageHandlerMissTransportConnectedNode)?;
                transport
                    .register_remote_info(&msg.answer, payload.relay.sender())
                    .await?;
            }

            MessageHandlerEvent::ForwardPayload => {
                if self
                    .get_and_check_transport(payload.relay.destination)
                    .await
                    .is_some()
                {
                    self.forward_payload(payload, Some(payload.relay.destination))
                        .await?;
                } else {
                    self.forward_payload(payload, None).await?;
                }
            }

            MessageHandlerEvent::JoinDHT(did) => {
                self.dht.join(*did)?;
            }
            MessageHandlerEvent::SendDirectMessage(msg, did) => {
                self.send_direct_message(msg.clone(), *did).await?;
            }
            MessageHandlerEvent::SendMessage(msg, did) => {
                self.send_message(msg.clone(), *did).await?;
            }
            MessageHandlerEvent::SendReportMessage(msg) => {
                self.send_report_message(payload, msg.clone()).await?;
            }
            MessageHandlerEvent::ResetDestination(did) => {
                self.reset_destination(payload, *did).await?;
            }
            MessageHandlerEvent::StorageStore(vnode) => {
                <Self as ChordStorageInterface<1>>::storage_store(self, vnode.clone()).await?;
            }
        }
        Ok(vec![])
    }

    pub fn push_pending_transport(&self, transport: &Arc<Transport>) -> Result<()> {
        let mut pending = self
            .pending_transports
            .try_lock()
            .map_err(|_| Error::SwarmPendingTransTryLockFailed)?;
        pending.push(transport.to_owned());
        Ok(())
    }

    pub fn pop_pending_transport(&self, transport_id: uuid::Uuid) -> Result<()> {
        let mut pending = self
            .pending_transports
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
            .pending_transports
            .try_lock()
            .map_err(|_| Error::SwarmPendingTransTryLockFailed)?;
        Ok(pending.iter().cloned().collect::<Vec<_>>())
    }

    pub fn find_pending_transport(&self, id: uuid::Uuid) -> Result<Option<Arc<Transport>>> {
        let pending = self
            .pending_transports
            .try_lock()
            .map_err(|_| Error::SwarmPendingTransTryLockFailed)?;
        Ok(pending.iter().find(|x| x.id.eq(&id)).cloned())
    }

    pub async fn disconnect(&self, did: Did) -> Result<()> {
        tracing::info!("disconnect {:?}", did);
        self.dht.remove(did)?;
        if let Some((_address, trans)) = self.remove_transport(did) {
            trans.close().await?
        }
        Ok(())
    }

    pub async fn connect(&self, did: Did) -> Result<Arc<Transport>> {
        if let Some(t) = self.get_and_check_transport(did).await {
            return Ok(t);
        }

        let (transport, offer_msg) = self.prepare_transport_offer().await?;

        self.send_message(Message::ConnectNodeSend(offer_msg), did)
            .await?;

        Ok(transport)
    }

    pub async fn inspect(&self) -> SwarmInspect {
        SwarmInspect::inspect(self).await
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

    fn dht(&self) -> Arc<PeerRing> {
        Swarm::dht(self)
    }

    async fn do_send_payload(&self, did: Did, payload: MessagePayload<T>) -> Result<()> {
        #[cfg(test)]
        {
            println!("+++++++++++++++++++++++++++++++++");
            println!("node {:?}", self.dht.did);
            println!("Sent {:?}", payload.clone());
            println!("node {:?}", payload.relay.next_hop);
            println!("+++++++++++++++++++++++++++++++++");
        }

        let transport = self
            .get_and_check_transport(did)
            .await
            .ok_or(Error::SwarmMissDidInTable(did))?;

        tracing::debug!(
            "Try send {:?}, to node {:?} via transport {:?}",
            payload.clone(),
            payload.relay.next_hop,
            transport.id
        );

        let data = payload.to_bincode()?;

        transport.wait_for_data_channel_open().await?;
        let result = transport.send_message(&data).await;

        tracing::debug!(
            "Sent {:?}, to node {:?} via transport {:?}",
            payload.clone(),
            payload.relay.next_hop,
            transport.id
        );

        if let (Some(measure), did) = (&self.measure, payload.relay.next_hop) {
            if result.is_ok() {
                measure.incr(did, MeasureCounter::Sent).await
            } else {
                measure.incr(did, MeasureCounter::FailedToSend).await
            }
        }

        result
    }
}

#[cfg(not(feature = "wasm"))]
impl Swarm {
    pub async fn listen(self: Arc<Self>) {
        loop {
            self.listen_once().await;
        }
    }
}

#[cfg(feature = "wasm")]
impl Swarm {
    pub async fn listen(self: Arc<Self>) {
        let func = move || {
            let this = self.clone();
            wasm_bindgen_futures::spawn_local(Box::pin(async move {
                this.listen_once().await;
            }));
        };
        crate::poll!(func, 10);
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
pub mod tests {
    use tokio::time;
    use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;

    use super::*;
    use crate::ecc::SecretKey;
    #[cfg(not(feature = "dummy"))]
    use crate::transports::default::transport::tests::establish_connection;
    #[cfg(feature = "dummy")]
    use crate::transports::dummy::transport::tests::establish_connection;

    pub async fn new_swarm(key: SecretKey) -> Result<Swarm> {
        let stun = "stun://stun.l.google.com:19302";
        let storage =
            PersistenceStorage::new_with_path(PersistenceStorage::random_path("./tmp")).await?;
        SwarmBuilder::new(stun, storage).key(key).build()
    }

    #[tokio::test]
    async fn swarm_new_transport() -> Result<()> {
        let swarm = new_swarm(SecretKey::random()).await?;
        let transport = swarm.new_transport().await.unwrap();
        assert_eq!(
            transport.ice_connection_state().await.unwrap(),
            RTCIceConnectionState::New
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_swarm_register_and_get() -> Result<()> {
        let swarm1 = new_swarm(SecretKey::random()).await?;
        let swarm2 = new_swarm(SecretKey::random()).await?;

        assert!(swarm1.get_transport(swarm2.did()).is_none());
        assert!(swarm2.get_transport(swarm1.did()).is_none());

        let transport1 = swarm1.new_transport().await.unwrap();
        let transport2 = swarm2.new_transport().await.unwrap();

        establish_connection(&transport1, &transport2).await?;

        // Can register if connected
        swarm1.register(swarm2.did(), transport1.clone()).await?;
        swarm2.register(swarm1.did(), transport2.clone()).await?;

        // Check address transport pairs in transports
        let transport_1_to_2 = swarm1.get_transport(swarm2.did()).unwrap();
        let transport_2_to_1 = swarm2.get_transport(swarm1.did()).unwrap();

        assert!(Arc::ptr_eq(&transport_1_to_2, &transport1));
        assert!(Arc::ptr_eq(&transport_2_to_1, &transport2));

        Ok(())
    }

    #[tokio::test]
    async fn test_swarm_will_close_previous_transport() -> Result<()> {
        let swarm1 = new_swarm(SecretKey::random()).await?;
        let swarm2 = new_swarm(SecretKey::random()).await?;

        assert!(swarm1.get_transport(swarm2.did()).is_none());

        let transport0 = swarm1.new_transport().await.unwrap();
        let transport1 = swarm1.new_transport().await.unwrap();

        let transport_2_to_0 = swarm2.new_transport().await.unwrap();
        let transport_2_to_1 = swarm2.new_transport().await.unwrap();

        establish_connection(&transport0, &transport_2_to_0).await?;
        establish_connection(&transport1, &transport_2_to_1).await?;

        swarm1.register(swarm2.did(), transport0.clone()).await?;
        swarm1.register(swarm2.did(), transport1.clone()).await?;

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
