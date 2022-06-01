#![warn(missing_docs)]
//! Processor of rings-node jsonrpc-server.
use crate::{
    error::{Error, Result},
    jsonrpc::{method, response::TransportAndIce},
    jsonrpc_client::SimpleClient,
    prelude::rings_core::{
        message::{Encoded, Message, MessageHandler},
        prelude::{
            uuid,
            web3::{contract::tokens::Tokenizable, ethabi::Token, types::Address},
            RTCSdpType,
        },
        swarm::{Swarm, TransportManager},
        transports::Transport,
        types::ice_transport::{IceTransport, IceTrickleScheme},
    },
};
#[cfg(feature = "client")]
use jsonrpc_core::Metadata;
use std::{str::FromStr, sync::Arc};

/// Processor for rings-node jsonrpc server
#[derive(Clone)]
pub struct Processor {
    /// a swarm instance
    pub swarm: Arc<Swarm>,
    /// a msg_handler instance
    pub msg_handler: Arc<MessageHandler>,
}

#[cfg(feature = "client")]
impl Metadata for Processor {}

impl From<(Arc<Swarm>, Arc<MessageHandler>)> for Processor {
    fn from((swarm, msg_handler): (Arc<Swarm>, Arc<MessageHandler>)) -> Self {
        Self { swarm, msg_handler }
    }
}

impl Processor {
    /// Create an Offer and waiting for connection.
    /// The process of manually handshake is:
    /// 1. PeerA: create_offer
    /// 2. PeerA: send the handshake info to PeerB.
    /// 3. PeerB: answer_offer
    /// 4. PeerB: send the handshake info to PeerA.
    /// 5. PeerA: accept_answer.
    pub async fn create_offer(&self) -> Result<(Arc<Transport>, Encoded)> {
        let transport = self
            .swarm
            .new_transport()
            .await
            .map_err(|_| Error::NewTransportError)?;
        let transport_cloned = transport.clone();
        let task = async move {
            let hs_info = transport_cloned
                .get_handshake_info(&self.swarm.session_manager, RTCSdpType::Offer)
                .await
                .map_err(Error::CreateOffer)?;
            self.swarm
                .push_pending_transport(&transport_cloned)
                .map_err(Error::PendingTransport)?;
            Ok(hs_info)
        };
        let hs_info = match task.await {
            Ok(hs_info) => (transport, hs_info),
            Err(e) => {
                transport.close().await.ok();
                return Err(e);
            }
        };
        Ok(hs_info)
    }

    /// Connect peer with remote rings-node jsonrpc server.
    /// * peer_url: the remote rings-node jsonrpc server url.
    pub async fn connect_peer_via_http(&self, peer_url: &str) -> Result<Arc<Transport>> {
        // request remote offer and sand answer to remote
        log::debug!("connect_peer_via_http: {}", peer_url);
        let transport = self
            .swarm
            .new_transport()
            .await
            .map_err(|_| Error::NewTransportError)?;
        let hs_info = self.do_connect_peer_via_http(&transport, peer_url).await;
        if let Err(e) = hs_info {
            transport
                .close()
                .await
                .map_err(Error::CloseTransportError)?;
            return Err(e);
        }
        Ok(transport)
    }

    async fn do_connect_peer_via_http(
        &self,
        transport: &Arc<Transport>,
        node_url: &str,
    ) -> Result<String> {
        let client = SimpleClient::new_with_url(node_url);
        let hs_info = transport
            .get_handshake_info(&self.swarm.session_manager, RTCSdpType::Offer)
            .await
            .map_err(Error::CreateOffer)?
            .to_string();
        log::debug!(
            "sending offer and candidate {:?} to {:?}",
            hs_info.to_owned(),
            node_url,
        );
        let resp = client
            .call_method(
                method::Method::AnswerOffer.as_str(),
                jsonrpc_core::Params::Array(vec![serde_json::json!(hs_info)]),
            )
            .await
            .map_err(|e| Error::RemoteRpcError(e.to_string()))?;
        let info: TransportAndIce =
            serde_json::from_value(resp).map_err(|_| Error::JsonDeserializeError)?;
        let addr = transport
            .register_remote_info(Encoded::from_encoded_str(info.ice.as_str()))
            .await
            .map_err(Error::RegisterIceError)?;
        // transport
        //     .connect_success_promise()
        //     .await
        //     .map_err(Error::ConnectError)?
        //     .await
        //     .map_err(Error::ConnectError)?;
        self.swarm
            .register(&addr, Arc::clone(transport))
            .await
            .map_err(Error::RegisterIceError)?;
        Ok(addr.to_string())
    }

    /// Answer an Offer.
    /// The process of manually handshake is:
    /// 1. PeerA: create_offer
    /// 2. PeerA: send the handshake info to PeerB.
    /// 3. PeerB: answer_offer
    /// 4. PeerB: send the handshake info to PeerA.
    /// 5. PeerA: accept_answer.
    pub async fn answer_offer(&self, ice_info: &str) -> Result<(Arc<Transport>, Encoded)> {
        log::info!("connect peer via ice: {}", ice_info);
        let transport = self.swarm.new_transport().await.map_err(|e| {
            log::error!("new_transport failed: {}", e);
            Error::NewTransportError
        })?;
        match self.handshake(&transport, ice_info).await {
            Ok(v) => Ok((transport, v)),
            Err(e) => {
                transport
                    .close()
                    .await
                    .map_err(Error::CloseTransportError)?;
                Err(e)
            }
        }
    }

    /// Connect peer with web3 address.
    /// There are 3 peers: PeerA, PeerB, PeerC.
    /// 1. PeerA has a connection with PeerB.
    /// 2. PeerC has a connection with PeerB.
    /// 3. PeerC can connect PeerA with PeerA's web3 address.
    pub async fn connect_with_address(&self, address: &Address) -> Result<()> {
        self.msg_handler
            .connect(address)
            .await
            .map_err(Error::ConnectWithAddressError)?;
        Ok(())
    }

    async fn handshake(&self, transport: &Arc<Transport>, data: &str) -> Result<Encoded> {
        // get offer from remote and send answer back
        let hs_info = Encoded::from_encoded_str(data);
        let addr = transport
            .register_remote_info(hs_info.to_owned())
            .await
            .map_err(Error::RegisterIceError)?;

        log::debug!("register: {}", addr);
        self.swarm
            .register(&addr, Arc::clone(transport))
            .await
            .map_err(Error::RegisterIceError)?;

        let hs_info = transport
            .get_handshake_info(&self.swarm.session_manager, RTCSdpType::Answer)
            .await
            .map_err(Error::CreateAnswer)?;
        log::debug!("answer hs_info: {:?}", hs_info);
        Ok(hs_info)
    }

    /// Accept an answer of a connection.
    /// The process of manually handshake is:
    /// 1. PeerA: create_offer
    /// 2. PeerA: send the handshake info to PeerB.
    /// 3. PeerB: answer_offer
    /// 4. PeerB: send the handshake info to PeerA.
    /// 5. PeerA: accept_answer.
    pub async fn accept_answer(&self, transport_id: &str, ice: &str) -> Result<Peer> {
        let ice = Encoded::from_encoded_str(ice);
        log::debug!("accept_answer/ice: {:?}, uuid: {}", ice, transport_id);
        let transport_id =
            uuid::Uuid::from_str(transport_id).map_err(|_| Error::InvalidTransportId)?;
        let transport = self
            .swarm
            .find_pending_transport(transport_id)
            .map_err(Error::PendingTransport)?
            .ok_or(Error::TransportNotFound)?;
        let addr = transport
            .register_remote_info(ice)
            .await
            .map_err(Error::RegisterIceError)?;
        transport
            .connect_success_promise()
            .await
            .map_err(Error::ConnectError)?
            .await
            .map_err(Error::ConnectError)?;
        self.swarm
            .register(&addr, transport.clone())
            .await
            .map_err(Error::RegisterIceError)?;
        if let Err(e) = self.swarm.pop_pending_transport(transport.id) {
            log::warn!("pop_pending_transport err: {}", e)
        };
        Ok(Peer::from((addr, transport)))
    }

    /// List all peers.
    pub async fn list_peers(&self) -> Result<Vec<Peer>> {
        let transports = self.swarm.get_transports();
        log::debug!(
            "addresses: {:?}",
            transports.iter().map(|(a, _b)| a).collect::<Vec<_>>()
        );
        let data = transports.iter().map(|x| x.into()).collect::<Vec<Peer>>();
        Ok(data)
    }

    /// Disconnect a peer with web3 address.
    pub async fn disconnect(&self, address: &str) -> Result<()> {
        let address = Address::from_str(address).map_err(|_| Error::InvalidAddress)?;
        let transport = self
            .swarm
            .get_transport(&address)
            .ok_or(Error::TransportNotFound)?;
        transport
            .close()
            .await
            .map_err(Error::CloseTransportError)?;
        Ok(())
    }

    /// List all pending transport.
    pub async fn list_pendings(&self) -> Result<Vec<Arc<Transport>>> {
        let pendings = self
            .swarm
            .pending_transports()
            .await
            .map_err(|_| Error::InternalError)?;
        Ok(pendings)
    }

    /// Close pending transport
    pub async fn close_pending_transport(&self, transport_id: &str) -> Result<()> {
        let transport_id =
            uuid::Uuid::from_str(transport_id).map_err(|_| Error::InvalidTransportId)?;
        let transport = self
            .swarm
            .find_pending_transport(transport_id)
            .map_err(|_| Error::TransportNotFound)?
            .ok_or(Error::TransportNotFound)?;
        if transport.is_connected().await {
            transport
                .close()
                .await
                .map_err(Error::CloseTransportError)?;
        }
        self.swarm
            .pop_pending_transport(transport_id)
            .map_err(Error::CloseTransportError)?;
        Ok(())
    }

    /// Send custom message to an address.
    pub async fn send_message<T>(&self, address: &str, msg: T) -> Result<()>
    where
        T: ToString,
    {
        log::info!("send_message, to: {}, text: {}", address, msg.to_string());
        let address = Address::from_str(address).map_err(|_| Error::InvalidAddress)?;
        let msg = Message::custom(msg.to_string().as_str(), &None).map_err(Error::SendMessage)?;
        self.msg_handler
            .send_message_default(&address, msg)
            .await
            .map_err(Error::SendMessage)?;
        Ok(())
    }
}

/// Peer struct
#[derive(Clone)]
pub struct Peer {
    /// web3 address of a peer.
    pub address: Token,
    /// transport of the connection.
    pub transport: Arc<Transport>,
}

impl From<(Address, Arc<Transport>)> for Peer {
    fn from((address, transport): (Address, Arc<Transport>)) -> Self {
        Self {
            address: address.into_token(),
            transport,
        }
    }
}

impl From<&(Address, Arc<Transport>)> for Peer {
    fn from((address, transport): &(Address, Arc<Transport>)) -> Self {
        Self {
            address: address.into_token(),
            transport: transport.clone(),
        }
    }
}

#[cfg(test)]
#[cfg(feature = "client")]
mod test {
    use super::*;
    use crate::prelude::rings_core::{
        dht::PeerRing, ecc::SecretKey, prelude::uuid, session::SessionManager,
    };
    use futures::lock::Mutex;

    fn new_processor() -> Processor {
        let key = SecretKey::random();

        let (auth, new_key) = SessionManager::gen_unsign_info(
            key.address(),
            Some(rings_core::session::Ttl::Never),
            None,
        )
        .unwrap();
        let sig = key.sign(&auth.to_string().unwrap()).to_vec();
        let session = SessionManager::new(&sig, &auth, &new_key);
        let swarm = Arc::new(Swarm::new(
            "stun://stun.l.google.com:19302",
            key.address(),
            session,
        ));

        let dht = Arc::new(Mutex::new(PeerRing::new(key.address().into())));
        let msg_handler = MessageHandler::new(dht, swarm.clone());
        (swarm, Arc::new(msg_handler)).into()
    }

    #[tokio::test]
    async fn test_processor_create_offer() {
        let processor = new_processor();
        let ti = processor.create_offer().await.unwrap();
        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 1);
        assert_eq!(pendings.get(0).unwrap().id.to_string(), ti.0.id.to_string());
    }

    #[tokio::test]
    async fn test_processor_list_pendings() {
        let processor = new_processor();
        let ti0 = processor.create_offer().await.unwrap();
        let ti1 = processor.create_offer().await.unwrap();
        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 2);
        let pending_ids = processor.list_pendings().await.unwrap();
        assert_eq!(pendings.len(), pending_ids.len());
        let ids = vec![ti0.0.id.to_string(), ti1.0.id.to_string()];
        for item in pending_ids {
            assert!(
                ids.contains(&item.id.to_string()),
                "id[{}] not in list",
                item.id
            );
        }
    }

    #[tokio::test]
    async fn test_processor_close_pending_transport() {
        let processor = new_processor();
        let ti0 = processor.create_offer().await.unwrap();
        let _ti1 = processor.create_offer().await.unwrap();
        let ti2 = processor.create_offer().await.unwrap();
        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 3);
        assert!(
            processor.close_pending_transport("abc").await.is_err(),
            "close_pending_transport() should be error"
        );
        let transport1 = processor
            .swarm
            .find_pending_transport(uuid::Uuid::from_str(ti0.0.id.to_string().as_str()).unwrap())
            .unwrap();
        assert!(transport1.is_some(), "transport_1 should be Some()");
        let transport1 = transport1.unwrap();
        assert!(
            processor
                .close_pending_transport(ti0.0.id.to_string().as_str())
                .await
                .is_ok(),
            "close_pending_transport({}) should be ok",
            ti0.0.id
        );
        assert!(!transport1.is_connected().await, "transport1 should closed");

        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 2);

        assert!(
            !pendings
                .iter()
                .any(|x| x.id.to_string() == ti0.0.id.to_string()),
            "transport[{}] should not in pending_transports",
            ti0.0.id
        );

        let transport2 = processor
            .swarm
            .find_pending_transport(uuid::Uuid::from_str(ti2.0.id.to_string().as_str()).unwrap())
            .unwrap();
        assert!(transport2.is_some(), "transport2 should be Some()");
        let transport2 = transport2.unwrap();
        assert!(
            processor
                .close_pending_transport(ti2.0.id.to_string().as_str())
                .await
                .is_ok(),
            "close_pending_transport({}) should be ok",
            ti0.0.id
        );
        assert!(!transport2.is_connected().await, "transport2 should closed");

        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 1);

        assert!(
            !pendings
                .iter()
                .any(|x| x.id.to_string() == ti2.0.id.to_string()),
            "transport[{}] should not in pending_transports",
            ti0.0.id
        );
    }
}
