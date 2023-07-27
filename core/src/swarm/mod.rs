#![warn(missing_docs)]
//! Tranposrt management
mod builder;
mod impls;
mod types;

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

use async_recursion::async_recursion;
use async_trait::async_trait;
pub use builder::SwarmBuilder;
use rings_derive::JudgeConnection;
use serde::de::DeserializeOwned;
use serde::Serialize;
pub use types::MeasureImpl;
pub use types::WrappedDid;

use crate::channels::Channel;
use crate::dht::types::Chord;
use crate::dht::CorrectChord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::error::Error;
use crate::error::Result;
use crate::inspect::SwarmInspect;
use crate::message;
use crate::message::types::NotifyPredecessorSend;
use crate::message::ChordStorageInterface;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessageHandlerEvent;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::session::SessionManager;
use crate::storage::MemStorage;
use crate::transports::manager::TransportHandshake;
use crate::transports::manager::TransportManager;
use crate::transports::Transport;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::TransportEvent;
use crate::types::ice_transport::IceServer;
use crate::types::ice_transport::IceTransportInterface;
use crate::types::ice_transport::IceTrickleScheme;

/// The transports and dht management.
#[derive(JudgeConnection)]
pub struct Swarm {
    /// A list to for store and manage pending_transport.
    pub(crate) pending_transports: Mutex<Vec<Arc<Transport>>>,
    /// Connected Transports.
    pub(crate) transports: MemStorage<Did, Arc<Transport>>,
    /// Configuration of ice_servers, including `TURN` and `STUN` server.
    pub(crate) ice_servers: Vec<IceServer>,
    /// Event channel for receive events from transport.
    pub(crate) transport_event_channel: Channel<TransportEvent>,
    /// Allow setup external address for webrtc transport.
    pub(crate) external_address: Option<String>,
    /// Reference of DHT.
    pub(crate) dht: Arc<PeerRing>,
    /// Implementationof measurement.
    pub(crate) measure: Option<MeasureImpl>,
    session_manager: SessionManager,
    message_handler: MessageHandler,
}

impl Swarm {
    /// Get did of self.
    pub fn did(&self) -> Did {
        self.dht.did
    }

    /// Get DHT(Distributed Hash Table) of self.
    pub fn dht(&self) -> Arc<PeerRing> {
        self.dht.clone()
    }

    /// Retrieves the session manager associated with the current instance.
    /// The session manager provides a segregated approach to manage private keys.
    /// It generates delegated secret keys for the bound entries of PKIs (Public Key Infrastructure).
    pub fn session_manager(&self) -> &SessionManager {
        &self.session_manager
    }

    /// Load message from a TransportEvent.
    async fn load_message(&self, ev: TransportEvent) -> Result<Option<MessagePayload<Message>>> {
        match ev {
            TransportEvent::DataChannelMessage(msg) => {
                let payload = MessagePayload::from_bincode(&msg)?;
                tracing::debug!("load message from channel: {:?}", payload);
                Ok(Some(payload))
            }
            TransportEvent::RegisterTransport((did, id)) => {
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
            TransportEvent::ConnectClosed((did, uuid)) => {
                if self.pop_pending_transport(uuid).is_ok() {
                    tracing::info!(
                        "[Swarm::ConnectClosed] Pending transport {:?} dropped",
                        uuid
                    );
                };

                if let Some(t) = self.get_transport(did) {
                    tracing::info!("[Swarm::ConnectClosed] removing transport {:?}", uuid);
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
        }
    }

    /// This method is required because web-sys components is not `Send`
    /// which means an async loop cannot running concurrency.
    pub async fn poll_message(&self) -> Option<MessagePayload<Message>> {
        let receiver = &self.transport_event_channel.receiver();
        match Channel::recv(receiver).await {
            Ok(Some(ev)) => match self.load_message(ev).await {
                Ok(Some(msg)) => Some(msg),
                Ok(None) => None,
                Err(_) => None,
            },
            Ok(None) => None,
            Err(e) => {
                tracing::error!("Failed on polling message, Error {}", e);
                None
            }
        }
    }

    /// This method is required because web-sys components is not `Send`
    /// This method will return events already consumed (landed), which is ok to be ignore.
    /// which means a listening loop cannot running concurrency.
    pub async fn listen_once(&self) -> Option<(MessagePayload<Message>, Vec<MessageHandlerEvent>)> {
        let payload = self.poll_message().await?;

        if !payload.verify() {
            tracing::error!("Cannot verify msg or it's expired: {:?}", payload);
            return None;
        }
        let events = self.message_handler.handle_message(&payload).await;

        match events {
            Ok(evs) => {
                self.handle_message_handler_events(&evs)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::error!(
                            "Swarm failed on handling event from message handler: {:#?}",
                            e
                        );
                    });
                Some((payload, evs))
            }
            Err(e) => {
                tracing::error!("Message handler failed on handling event: {:#?}", e);
                None
            }
        }
    }

    /// Event handler of Swarm.
    pub async fn handle_message_handler_event(
        &self,
        event: &MessageHandlerEvent,
    ) -> Result<Vec<MessageHandlerEvent>> {
        tracing::debug!("Handle message handler event: {:?}", event);
        match event {
            MessageHandlerEvent::Connect(did) => {
                let did = *did;
                if self.get_and_check_transport(did).await.is_none() && did != self.did() {
                    self.connect(did).await?;
                }
                Ok(vec![])
            }

            // Notify did with self.id
            MessageHandlerEvent::Notify(did) => {
                let msg =
                    Message::NotifyPredecessorSend(NotifyPredecessorSend { did: self.dht.did });
                Ok(vec![MessageHandlerEvent::SendMessage(msg, *did)])
            }

            MessageHandlerEvent::ConnectVia(did, next) => {
                let did = *did;
                if self.get_and_check_transport(did).await.is_none() && did != self.did() {
                    self.connect_via(did, *next).await?;
                }
                Ok(vec![])
            }

            MessageHandlerEvent::Disconnect(did) => {
                self.disconnect(*did).await?;
                Ok(vec![])
            }

            MessageHandlerEvent::AnswerOffer(relay, msg) => {
                let (_, answer) = self
                    .answer_remote_transport(relay.relay.origin_sender().to_owned(), msg)
                    .await?;

                Ok(vec![MessageHandlerEvent::SendReportMessage(
                    relay.clone(),
                    Message::ConnectNodeReport(answer),
                )])
            }

            MessageHandlerEvent::AcceptAnswer(sender, msg) => {
                let transport = self
                    .find_pending_transport(
                        uuid::Uuid::from_str(&msg.transport_uuid)
                            .map_err(|_| Error::InvalidTransportUuid)?,
                    )?
                    .ok_or(Error::MessageHandlerMissTransportConnectedNode)?;
                transport
                    .register_remote_info(&msg.answer, sender.to_owned())
                    .await?;
                Ok(vec![])
            }

            MessageHandlerEvent::ForwardPayload(payload, next_hop) => {
                if self
                    .get_and_check_transport(payload.relay.destination)
                    .await
                    .is_some()
                {
                    self.forward_payload(payload, Some(payload.relay.destination))
                        .await?;
                } else {
                    self.forward_payload(payload, *next_hop).await?;
                }
                Ok(vec![])
            }

            MessageHandlerEvent::JoinDHT(ctx, did) => {
                if cfg!(feature = "experimental") {
                    let wdid: WrappedDid = WrappedDid::new(self, *did);
                    let dht_ev = self.dht.join_then_sync(wdid).await?;
                    crate::message::handlers::dht::handle_dht_events(&dht_ev, ctx).await
                } else {
                    let dht_ev = self.dht.join(*did)?;
                    crate::message::handlers::dht::handle_dht_events(&dht_ev, ctx).await
                }
            }

            MessageHandlerEvent::SendDirectMessage(msg, dest) => {
                self.send_direct_message(msg.clone(), *dest).await?;
                Ok(vec![])
            }

            MessageHandlerEvent::SendMessage(msg, dest) => {
                self.send_message(msg.clone(), *dest).await?;
                Ok(vec![])
            }

            MessageHandlerEvent::SendReportMessage(payload, msg) => {
                self.send_report_message(payload, msg.clone()).await?;
                Ok(vec![])
            }

            MessageHandlerEvent::ResetDestination(payload, next_hop) => {
                self.reset_destination(payload, *next_hop).await?;
                Ok(vec![])
            }

            MessageHandlerEvent::StorageStore(vnode) => {
                <Self as ChordStorageInterface<1>>::storage_store(self, vnode.clone()).await?;
                Ok(vec![])
            }
        }
    }

    /// Batch handle events
    #[cfg_attr(feature = "wasm", async_recursion(?Send))]
    #[cfg_attr(not(feature = "wasm"), async_recursion)]
    pub async fn handle_message_handler_events(
        &self,
        events: &Vec<MessageHandlerEvent>,
    ) -> Result<()> {
        match events.as_slice() {
            [] => Ok(()),
            [x] => {
                let evs = self.handle_message_handler_event(x).await?;
                self.handle_message_handler_events(&evs).await
            }
            [x, xs @ ..] => {
                self.handle_message_handler_events(&vec![x.clone()]).await?;
                self.handle_message_handler_events(&xs.to_vec()).await
            }
        }
    }

    /// Push a pending transport to pending list.
    pub fn push_pending_transport(&self, transport: &Arc<Transport>) -> Result<()> {
        let mut pending = self
            .pending_transports
            .try_lock()
            .map_err(|_| Error::SwarmPendingTransTryLockFailed)?;
        pending.push(transport.to_owned());
        Ok(())
    }

    /// Pop a pending trainsport from pending list.
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

    /// List all the pending transports.
    pub async fn pending_transports(&self) -> Result<Vec<Arc<Transport>>> {
        let pending = self
            .pending_transports
            .try_lock()
            .map_err(|_| Error::SwarmPendingTransTryLockFailed)?;
        Ok(pending.iter().cloned().collect::<Vec<_>>())
    }

    /// Find a pending transport from pending list by uuid.
    pub fn find_pending_transport(&self, id: uuid::Uuid) -> Result<Option<Arc<Transport>>> {
        let pending = self
            .pending_transports
            .try_lock()
            .map_err(|_| Error::SwarmPendingTransTryLockFailed)?;
        Ok(pending.iter().find(|x| x.id.eq(&id)).cloned())
    }

    /// Disconnect a transport. There are three steps:
    /// 1) remove from DHT;
    /// 2) remove from transport pool;
    /// 3) close the transport connection;
    pub async fn disconnect(&self, did: Did) -> Result<()> {
        JudgeConnection::disconnect(self, did).await
    }

    /// Connect a given Did. It the did is managed by swarm transport pool, return directly,
    /// else try prepare offer and establish connection by dht.
    /// This function may returns a pending transport or connected transport.
    pub async fn connect(&self, did: Did) -> Result<Arc<Transport>> {
        JudgeConnection::connect(self, did).await
    }

    /// Similar to connect, but this function will try connect a Did by given hop.
    pub async fn connect_via(&self, did: Did, next_hop: Did) -> Result<Arc<Transport>> {
        JudgeConnection::connect_via(self, did, next_hop).await
    }

    /// Check the status of swarm
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

        if result.is_ok() {
            self.record_sent(payload.relay.next_hop).await
        } else {
            self.record_sent_failed(payload.relay.next_hop).await
        }

        result
    }
}

#[cfg(not(feature = "wasm"))]
impl Swarm {
    /// Listener for native envirement, It will just launch a loop.
    pub async fn listen(self: Arc<Self>) {
        loop {
            self.listen_once().await;
        }
    }
}

#[cfg(feature = "wasm")]
impl Swarm {
    /// Listener for browser envirement, the implementation is based on  js_sys::window.set_timeout.
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
    use crate::storage::PersistenceStorage;
    #[cfg(not(feature = "dummy"))]
    use crate::transports::default::transport::tests::establish_connection;
    #[cfg(feature = "dummy")]
    use crate::transports::dummy::transport::tests::establish_connection;

    pub async fn new_swarm(key: SecretKey) -> Result<Swarm> {
        let stun = "stun://stun.l.google.com:19302";
        let storage =
            PersistenceStorage::new_with_path(PersistenceStorage::random_path("./tmp")).await?;
        let session_manager = SessionManager::new_with_seckey(&key)?;
        Ok(SwarmBuilder::new(stun, storage, session_manager).build())
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
