use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use async_lock::RwLock as AsyncRwLock;
use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use itertools::Itertools;
use lazy_static::lazy_static;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;

use crate::channels::Channel as AcChannel;
use crate::dht::Did;
use crate::ecc::PublicKey;
use crate::err::Error;
use crate::err::Result;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::MessagePayload;
use crate::peer::PeerService;
use crate::session::SessionManager;
use crate::transports::helper::Promise;
use crate::transports::helper::State;
use crate::transports::helper::TricklePayload;
use crate::types::channel::Channel;
use crate::types::channel::Event;
use crate::types::ice_transport::IceServer;
use crate::types::ice_transport::IceTransportInterface;
use crate::types::ice_transport::IceTrickleScheme;

type EventSender = <AcChannel<Event> as Channel<Event>>::Sender;

/// Dummy transport use for test only.
#[derive(Default)]
pub struct DummyTransportHub {
    pub senders: DashMap<uuid::Uuid, EventSender>,
    pub dids: DashMap<uuid::Uuid, Did>,
}

lazy_static! {
    static ref HUB: DummyTransportHub = DummyTransportHub::default();
}

#[derive(Clone)]
pub struct DummyTransport {
    pub id: uuid::Uuid,
    remote_id: Arc<Mutex<Option<uuid::Uuid>>>,
    event_sender: EventSender,
    ice_connection_state: Arc<Mutex<Option<RTCIceConnectionState>>>,
    public_key: Arc<AsyncRwLock<Option<PublicKey>>>,
    services: Arc<AsyncRwLock<Vec<PeerService>>>,
}

impl PartialEq for DummyTransport {
    fn eq(&self, other: &Self) -> bool {
        self.id.eq(&other.id)
    }
}

#[async_trait]
impl IceTransportInterface<Event, AcChannel<Event>> for DummyTransport {
    type IceConnectionState = RTCIceConnectionState;

    fn new(event_sender: EventSender) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            remote_id: Arc::new(Mutex::new(None)),
            event_sender,
            ice_connection_state: Arc::new(Mutex::new(None)),
            public_key: Arc::new(AsyncRwLock::new(None)),
            services: Default::default(),
        }
    }

    async fn start(
        &mut self,
        _ice_server: Vec<IceServer>,
        _external_ip: Option<String>,
    ) -> Result<&Self> {
        let mut ice_connection_state = self.ice_connection_state.lock().unwrap();
        *ice_connection_state = Some(RTCIceConnectionState::New);
        HUB.senders.insert(self.id, self.event_sender.clone());
        Ok(self)
    }

    async fn apply_callback(&self) -> Result<&Self> {
        Ok(self)
    }

    async fn close(&self) -> Result<()> {
        {
            let mut ice_connection_state = self.ice_connection_state.lock().unwrap();
            *ice_connection_state = Some(RTCIceConnectionState::Closed);
        }

        self.event_sender
            .send(Event::ConnectClosed((
                self.pubkey().await.address().into(),
                self.id,
            )))
            .await
            .unwrap();

        Ok(())
    }

    async fn ice_connection_state(&self) -> Option<Self::IceConnectionState> {
        *self.ice_connection_state.lock().unwrap()
    }

    async fn is_disconnected(&self) -> bool {
        matches!(
            self.ice_connection_state().await,
            Some(Self::IceConnectionState::Failed)
                | Some(Self::IceConnectionState::Disconnected)
                | Some(Self::IceConnectionState::Closed)
        )
    }

    async fn is_connected(&self) -> bool {
        self.ice_connection_state()
            .await
            .map(|s| s == RTCIceConnectionState::Connected)
            .unwrap_or(false)
    }

    async fn pubkey(&self) -> PublicKey {
        self.public_key.read().await.unwrap()
    }

    async fn services(&self) -> Vec<PeerService> {
        self.services.read().await.clone()
    }

    async fn send_message(&self, msg: &Bytes) -> Result<()> {
        self.remote_sender()
            .send(Event::DataChannelMessage(msg.to_vec()))
            .await
            .unwrap();
        Ok(())
    }
}

#[async_trait]
impl IceTrickleScheme for DummyTransport {
    // https://datatracker.ietf.org/doc/html/rfc5245
    // 1. Send (SdpOffer, IceCandidates) to remote
    // 2. Recv (SdpAnswer, IceCandidate) From Remote

    type SdpType = RTCSdpType;

    async fn get_handshake_info(
        &self,
        session_manager: &SessionManager,
        _kind: RTCSdpType,
        services: HashSet<PeerService>,
    ) -> Result<Encoded> {
        let data = TricklePayload {
            sdp: serde_json::to_string(&self.id).unwrap(),
            candidates: vec![],
            services: services.into_iter().collect_vec(),
        };
        let resp =
            MessagePayload::new_direct(data, session_manager, session_manager.authorizer()?)?;
        Ok(resp.encode()?)
    }

    async fn register_remote_info(&self, data: Encoded) -> Result<Did> {
        let data: MessagePayload<TricklePayload> = data.decode()?;
        match data.verify() {
            true => {
                {
                    let sdp = serde_json::from_str::<uuid::Uuid>(&data.data.sdp)
                        .map_err(Error::Deserialize)?;

                    let mut remote_id = self.remote_id.lock().unwrap();
                    *remote_id = Some(sdp);
                }

                {
                    let mut ice_connection_state = self.ice_connection_state.lock().unwrap();
                    *ice_connection_state = Some(RTCIceConnectionState::Connected);
                }

                if let Ok(public_key) = data.origin_verification.session.authorizer_pubkey() {
                    let mut pk = self.public_key.write().await;
                    *pk = Some(public_key);
                };

                let local_did = self.pubkey().await.address().into();
                HUB.dids.insert(self.id, local_did);
                self.event_sender
                    .send(Event::RegisterTransport((local_did, self.id)))
                    .await
                    .unwrap_or_else(|e| tracing::warn!("failed to send register event: {:?}", e));

                Ok(data.addr)
            }
            _ => Err(Error::VerifySignatureFailed),
        }
    }

    async fn wait_for_connected(&self) -> Result<()> {
        let promise = self.connect_success_promise().await?;
        promise.await
    }
}

impl DummyTransport {
    pub async fn connect_success_promise(&self) -> Result<Promise> {
        let state = State {
            completed: true,
            successed: Some(true),
            ..Default::default()
        };
        let promise = Promise(Arc::new(Mutex::new(state)));
        Ok(promise)
    }

    pub async fn wait_for_data_channel_open(&self) -> Result<()> {
        Ok(())
    }

    pub fn remote_id(&self) -> uuid::Uuid {
        self.remote_id.lock().unwrap().unwrap()
    }

    pub fn remote_sender(&self) -> EventSender {
        HUB.senders.get(&self.remote_id()).unwrap().clone()
    }
}

#[cfg(test)]
pub mod tests {
    use std::str::FromStr;

    use super::DummyTransport as Transport;
    use super::*;
    use crate::ecc::SecretKey;
    use crate::session::SessionManager;
    use crate::types::ice_transport::IceServer;

    async fn prepare_transport() -> Result<Transport> {
        let ch = Arc::new(AcChannel::new());
        let mut trans = Transport::new(ch.sender());

        let stun = IceServer::from_str("stun://stun.l.google.com:19302").unwrap();
        trans
            .start(vec![stun], None)
            .await?
            .apply_callback()
            .await?;
        Ok(trans)
    }

    pub async fn establish_connection(
        transport1: &Transport,
        transport2: &Transport,
    ) -> Result<()> {
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );

        // Generate key pairs for signing and verification
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();

        // Generate Session associated to Keys
        let sm1 = SessionManager::new_with_seckey(&key1, None)?;
        let sm2 = SessionManager::new_with_seckey(&key2, None)?;

        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::New)
        );

        // Peer 1 try to connect peer 2
        let handshake_info1 = transport1
            .get_handshake_info(&sm1, RTCSdpType::Offer, HashSet::new())
            .await?;

        // Peer 2 got offer then register
        let addr1 = transport2.register_remote_info(handshake_info1).await?;
        assert_eq!(addr1, key1.address().into());

        // Peer 2 create answer
        let handshake_info2 = transport2
            .get_handshake_info(&sm2, RTCSdpType::Answer, HashSet::new())
            .await?;

        // Peer 1 got answer then register
        let addr2 = transport1.register_remote_info(handshake_info2).await?;
        assert_eq!(addr2, key2.address().into());

        let promise_1 = transport1.connect_success_promise().await?;
        let promise_2 = transport2.connect_success_promise().await?;
        promise_1.await?;
        promise_2.await?;

        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RTCIceConnectionState::Connected)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RTCIceConnectionState::Connected)
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_ice_connection_establish() -> Result<()> {
        let transport1 = prepare_transport().await?;
        let transport2 = prepare_transport().await?;

        establish_connection(&transport1, &transport2).await?;

        Ok(())
    }
}
