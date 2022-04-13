use super::{request, response::TransportAndIce};
use crate::error::{Error, Result};
use bns_core::{
    message::Encoded,
    swarm::{Swarm, TransportManager},
    transports::Transport,
    types::ice_transport::{IceTransport, IceTrickleScheme},
};
use jsonrpc_core::Metadata;
use jsonrpc_core_client::RawClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use std::{str::FromStr, sync::Arc};
use web3::types::{Address, H160};
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;

#[derive(Clone)]
pub struct Processor {
    pub swarm: Arc<Swarm>,
}

impl Metadata for Processor {}

impl From<Arc<Swarm>> for Processor {
    fn from(swarm: Arc<Swarm>) -> Self {
        Self { swarm }
    }
}

impl Processor {
    pub async fn create_offer(&self) -> Result<TransportAndIce> {
        let transport = self
            .swarm
            .new_transport()
            .await
            .map_err(|_| Error::NewTransportError)?;
        let id = transport.id;
        let transport_cloned = transport.clone();
        let task = async move {
            let hs_info = transport_cloned
                .get_handshake_info(self.swarm.session(), RTCSdpType::Offer)
                .await
                .map_err(Error::CreateOffer)?
                .to_string();
            self.swarm
                .push_pending_transport(&transport_cloned)
                .map_err(Error::PendingTransport)?;
            Ok(hs_info)
        };
        let hs_info = match task.await {
            Ok(hs_info) => TransportAndIce::new(id.to_string().as_str(), hs_info.as_str()),
            Err(e) => {
                transport.close().await.ok();
                return Err(e);
            }
        };
        Ok(hs_info)
    }

    pub async fn connect_peer_via_http(&self, peer_url: &str) -> Result<String> {
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
        Ok(transport.id.to_string())
    }

    async fn do_connect_peer_via_http(
        &self,
        transport: &Arc<Transport>,
        node_url: &str,
    ) -> Result<String> {
        let client = new_client(node_url)
            .await
            .map_err(|e| Error::RemoteRpcError(e.to_string()))?;
        let hs_info = transport
            .get_handshake_info(self.swarm.session(), RTCSdpType::Offer)
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
                request::Method::ConnectPeerViaIce.as_str(),
                jsonrpc_core::Params::Array(vec![json!(hs_info)]),
            )
            .await
            .map_err(|e| Error::RemoteRpcError(e.to_string()))?;
        log::debug!("resp: {}", resp);
        let info: TransportAndIce =
            serde_json::from_value(resp).map_err(|_| Error::JsonDeserializeError)?;
        let addr = transport
            .register_remote_info(Encoded::from_encoded_str(info.ice.as_str()))
            .await
            .map_err(Error::RegisterIceError)?;
        self.swarm
            .register(&addr, Arc::clone(transport))
            .await
            .map_err(Error::RegisterIceError)?;
        Ok(addr.to_string())
    }

    pub async fn connect_peer_via_ice(&self, ice_info: &str) -> Result<TransportAndIce> {
        log::debug!("connect peer via ice: {}", ice_info);
        let transport = self.swarm.new_transport().await.map_err(|e| {
            log::error!("new_transport failed: {}", e);
            Error::NewTransportError
        })?;
        match self.handshake(&transport, ice_info).await {
            Ok(v) => Ok(TransportAndIce::new(
                transport.id.to_string().as_str(),
                v.as_str(),
            )),
            Err(e) => {
                transport
                    .close()
                    .await
                    .map_err(Error::CloseTransportError)?;
                Err(e)
            }
        }
    }

    async fn handshake(&self, transport: &Arc<Transport>, data: &str) -> Result<String> {
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
            .get_handshake_info(self.swarm.session(), RTCSdpType::Answer)
            .await
            .map_err(Error::CreateAnswer)?
            .to_string();
        log::debug!("answer hs_info: {}", hs_info);
        Ok(hs_info)
    }

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
        self.swarm
            .register(&addr, transport.clone())
            .await
            .map_err(Error::RegisterIceError)?;
        if let Err(e) = self.swarm.pop_pending_transport(transport.id) {
            log::warn!("pop_pending_transport err: {}", e)
        };
        Ok(Peer::from((addr, transport)))
    }

    pub async fn list_peers(&self) -> Result<Vec<Peer>> {
        let transports = self.swarm.get_transports();
        log::debug!(
            "addresses: {:?}",
            transports.iter().map(|(a, _b)| a).collect::<Vec<_>>()
        );
        let data = transports.iter().map(|x| x.into()).collect::<Vec<Peer>>();
        Ok(data)
    }

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

    pub async fn list_pendings(&self) -> Result<Vec<String>> {
        let pendings = self
            .swarm
            .pending_transports()
            .await
            .map_err(|_| Error::InternalError)?;
        Ok(pendings.iter().map(|x| x.id.to_string()).collect())
    }

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
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Peer {
    pub address: String,
    pub transport_id: String,
}

impl Peer {
    pub fn to_json_vec(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|_| Error::JsonSerializeError)
    }

    pub fn to_json_obj(&self) -> Result<JsonValue> {
        serde_json::to_value(self).map_err(|_| Error::JsonSerializeError)
    }

    pub fn base64_encode(&self) -> Result<String> {
        Ok(base64::encode(self.to_json_vec()?))
    }
}

impl From<(H160, Arc<Transport>)> for Peer {
    fn from((address, transport): (H160, Arc<Transport>)) -> Self {
        Self {
            address: address.to_string(),
            transport_id: transport.id.to_string(),
        }
    }
}

impl From<&(H160, Arc<Transport>)> for Peer {
    fn from((address, transport): &(H160, Arc<Transport>)) -> Self {
        Self {
            address: address.to_string(),
            transport_id: transport.id.to_string(),
        }
    }
}

pub async fn new_client(url: &str) -> Result<RawClient> {
    let c: RawClient = jsonrpc_core_client::transports::http::connect(url)
        .await
        .map_err(|e| Error::RemoteRpcError(e.to_string()))?;
    Ok(c)
}

#[cfg(test)]
mod test {
    use super::*;
    use bns_core::{ecc::SecretKey, session::SessionManager};

    fn new_processor() -> Processor {
        let key = SecretKey::random();

        let (auth, key) =
            SessionManager::gen_unsign_info(key.address(), Some(bns_core::session::Ttl::Never))
                .unwrap();
        let sig = key.sign(&auth.to_string().unwrap()).to_vec();
        let session = SessionManager::new(&sig, &auth, &key);
        let swarm = Arc::new(Swarm::new(
            "stun://stun.l.google.com:19302",
            key.address(),
            session,
        ));
        swarm.into()
    }

    #[tokio::test]
    async fn test_processor_create_offer() {
        let processor = new_processor();
        let ti = processor.create_offer().await.unwrap();
        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 1);
        assert_eq!(pendings.get(0).unwrap().id.to_string(), ti.transport_id);
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
        let ids = vec![ti0.transport_id, ti1.transport_id];
        for item in pending_ids {
            assert!(ids.contains(&item), "id[{}] not in list", item);
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
            .find_pending_transport(uuid::Uuid::from_str(ti0.transport_id.as_str()).unwrap())
            .unwrap();
        assert!(transport1.is_some(), "transport_1 should be Some()");
        let transport1 = transport1.unwrap();
        assert!(
            processor
                .close_pending_transport(ti0.transport_id.as_str())
                .await
                .is_ok(),
            "close_pending_transport({}) should be ok",
            ti0.transport_id
        );
        assert!(!transport1.is_connected().await, "transport1 should closed");

        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 2);

        assert!(
            !pendings
                .iter()
                .any(|x| x.id.to_string() == ti0.transport_id),
            "transport[{}] should not in pending_transports",
            ti0.transport_id
        );

        let transport2 = processor
            .swarm
            .find_pending_transport(uuid::Uuid::from_str(ti2.transport_id.as_str()).unwrap())
            .unwrap();
        assert!(transport2.is_some(), "transport2 should be Some()");
        let transport2 = transport2.unwrap();
        assert!(
            processor
                .close_pending_transport(ti2.transport_id.as_str())
                .await
                .is_ok(),
            "close_pending_transport({}) should be ok",
            ti0.transport_id
        );
        assert!(!transport2.is_connected().await, "transport2 should closed");

        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 1);

        assert!(
            !pendings
                .iter()
                .any(|x| x.id.to_string() == ti2.transport_id),
            "transport[{}] should not in pending_transports",
            ti0.transport_id
        );
    }
}
