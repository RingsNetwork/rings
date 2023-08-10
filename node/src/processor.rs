#![warn(missing_docs)]

//! Processor of rings-node jsonrpc-server.

use std::str::FromStr;
use std::sync::Arc;

use futures::future::Join;
use futures::Future;
#[cfg(feature = "node")]
use jsonrpc_core::Metadata;
use rings_core::message::MessagePayload;
use serde::Deserialize;
use serde::Serialize;

use crate::backend::types::BackendMessage;
use crate::backend::types::MessageType;
use crate::consts::DATA_REDUNDANT;
use crate::error::Error;
use crate::error::Result;
use crate::measure::PeriodicMeasure;
use crate::prelude::http;
use crate::prelude::jsonrpc_client::SimpleClient;
use crate::prelude::jsonrpc_core;
use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_core::dht::Stabilization;
use crate::prelude::rings_core::dht::TStabilize;
use crate::prelude::rings_core::message::Decoder;
use crate::prelude::rings_core::message::Encoded;
use crate::prelude::rings_core::message::Encoder;
use crate::prelude::rings_core::message::Message;
use crate::prelude::rings_core::message::PayloadSender;
use crate::prelude::rings_core::prelude::uuid;
use crate::prelude::rings_core::prelude::web3::contract::tokens::Tokenizable;
use crate::prelude::rings_core::prelude::web3::ethabi::Token;
use crate::prelude::rings_core::storage::PersistenceStorage;
use crate::prelude::rings_core::swarm::MeasureImpl;
use crate::prelude::rings_core::swarm::Swarm;
use crate::prelude::rings_core::swarm::SwarmBuilder;
use crate::prelude::rings_core::transports::manager::TransportHandshake;
use crate::prelude::rings_core::transports::manager::TransportManager;
use crate::prelude::rings_core::transports::Transport;
use crate::prelude::rings_core::types::ice_transport::IceTransportInterface;
use crate::prelude::rings_rpc::method;
use crate::prelude::rings_rpc::response;
use crate::prelude::rings_rpc::types::HttpRequest;
use crate::prelude::rings_rpc::types::Timeout;
use crate::prelude::vnode;
use crate::prelude::wasm_export;
use crate::prelude::CallbackFn;
use crate::prelude::ChordStorageInterface;
use crate::prelude::ChordStorageInterfaceCacheChecker;
use crate::prelude::CustomMessage;
use crate::prelude::SessionSk;

/// ProcessorConfig is usually serialized as json or yaml.
/// There is a `from_config` method in [ProcessorBuilder] used to initialize the Builder with a serialized ProcessorConfig.
#[derive(Clone)]
#[wasm_export]
pub struct ProcessorConfig {
    /// ICE servers for webrtc transport.
    ice_servers: String,
    /// External address for webrtc transport.
    external_address: Option<String>,
    /// [SessionSk].
    session_sk: SessionSk,
    /// Stabilization timeout.
    stabilize_timeout: usize,
}

#[wasm_export]
impl ProcessorConfig {
    /// Creates a new `ProcessorConfig` instance without an external address.
    pub fn new(ice_servers: String, session_sk: SessionSk, stabilize_timeout: usize) -> Self {
        Self {
            ice_servers,
            external_address: None,
            session_sk,
            stabilize_timeout,
        }
    }

    /// Creates a new `ProcessorConfig` instance with an external address.
    pub fn new_with_ext_addr(
        ice_servers: String,
        session_sk: SessionSk,
        stabilize_timeout: usize,
        external_address: String,
    ) -> Self {
        Self {
            ice_servers,
            external_address: Some(external_address),
            session_sk,
            stabilize_timeout,
        }
    }

    /// Return associated [SessionSk].
    pub fn session_sk(&self) -> SessionSk {
        self.session_sk.clone()
    }
}

impl FromStr for ProcessorConfig {
    type Err = Error;
    /// Reveal config from serialized string.
    fn from_str(ser: &str) -> Result<Self> {
        serde_yaml::from_str::<ProcessorConfig>(ser).map_err(Error::SerdeYamlError)
    }
}

/// `ProcessorConfigSerialized` is a serialized version of `ProcessorConfig`.
/// Instead of storing the `SessionSk` instance, it stores the dumped string representation of the session secret key.
#[derive(Serialize, Deserialize, Clone)]
#[wasm_export]
pub struct ProcessorConfigSerialized {
    /// A string representing ICE servers for WebRTC transport.
    ice_servers: String,
    /// An optional string representing the external address for WebRTC transport.
    external_address: Option<String>,
    /// A string representing the dumped `SessionSk`.
    session_sk: String,
    /// An unsigned integer representing the stabilization timeout.
    stabilize_timeout: usize,
}

impl ProcessorConfigSerialized {
    /// Creates a new `ProcessorConfigSerialized` instance without an external address.
    pub fn new(ice_servers: String, session_sk: String, stabilize_timeout: usize) -> Self {
        Self {
            ice_servers,
            external_address: None,
            session_sk,
            stabilize_timeout,
        }
    }

    /// Creates a new `ProcessorConfigSerialized` instance with an external address.
    pub fn new_with_ext_addr(
        ice_servers: String,
        session_sk: String,
        stabilize_timeout: usize,
        external_address: String,
    ) -> Self {
        Self {
            ice_servers,
            external_address: Some(external_address),
            session_sk,
            stabilize_timeout,
        }
    }
}

impl TryFrom<ProcessorConfig> for ProcessorConfigSerialized {
    type Error = Error;
    fn try_from(ins: ProcessorConfig) -> Result<Self> {
        Ok(Self {
            ice_servers: ins.ice_servers.clone(),
            external_address: ins.external_address.clone(),
            session_sk: ins
                .session_sk
                .dump()
                .map_err(|e| Error::CoreError(e.to_string()))?,
            stabilize_timeout: ins.stabilize_timeout,
        })
    }
}

impl TryFrom<ProcessorConfigSerialized> for ProcessorConfig {
    type Error = Error;
    fn try_from(ins: ProcessorConfigSerialized) -> Result<Self> {
        Ok(Self {
            ice_servers: ins.ice_servers.clone(),
            external_address: ins.external_address.clone(),
            session_sk: SessionSk::from_str(&ins.session_sk)
                .map_err(|e| Error::CoreError(e.to_string()))?,
            stabilize_timeout: ins.stabilize_timeout,
        })
    }
}

impl Serialize for ProcessorConfig {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        let ins: ProcessorConfigSerialized = self
            .clone()
            .try_into()
            .map_err(|e: Error| serde::ser::Error::custom(e.to_string()))?;
        ProcessorConfigSerialized::serialize(&ins, serializer)
    }
}

impl<'de> serde::de::Deserialize<'de> for ProcessorConfig {
    fn deserialize<D>(deserializer: D) -> core::result::Result<Self, D::Error>
    where D: serde::Deserializer<'de> {
        match ProcessorConfigSerialized::deserialize(deserializer) {
            Ok(ins) => {
                let cfg: ProcessorConfig = ins
                    .try_into()
                    .map_err(|e: Error| serde::de::Error::custom(e.to_string()))?;
                Ok(cfg)
            }
            Err(e) => Err(e),
        }
    }
}

/// ProcessorBuilder is used to initialize a [Processor] instance.
pub struct ProcessorBuilder {
    ice_servers: String,
    external_address: Option<String>,
    session_sk: SessionSk,
    storage: Option<PersistenceStorage>,
    measure: Option<MeasureImpl>,
    message_callback: Option<CallbackFn>,
    stabilize_timeout: usize,
}

/// Processor for rings-node jsonrpc server
#[derive(Clone)]
pub struct Processor {
    /// a swarm instance
    pub swarm: Arc<Swarm>,
    /// a stabilization instance,
    pub stabilization: Arc<Stabilization>,
}

impl ProcessorBuilder {
    /// initialize a [ProcessorBuilder] with a serialized [ProcessorConfig].
    pub fn from_serialized(config: &str) -> Result<Self> {
        let config =
            serde_yaml::from_str::<ProcessorConfig>(config).map_err(Error::SerdeYamlError)?;
        Self::from_config(&config)
    }

    /// initialize a [ProcessorBuilder] with a [ProcessorConfig].
    pub fn from_config(config: &ProcessorConfig) -> Result<Self> {
        Ok(Self {
            ice_servers: config.ice_servers.clone(),
            external_address: config.external_address.clone(),
            session_sk: config.session_sk.clone(),
            storage: None,
            measure: None,
            message_callback: None,
            stabilize_timeout: config.stabilize_timeout,
        })
    }

    /// Set the storage for the processor.
    pub fn storage(mut self, storage: PersistenceStorage) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Set the measure for the processor.
    pub fn measure(mut self, implement: PeriodicMeasure) -> Self {
        self.measure = Some(Box::new(implement));
        self
    }

    /// Set the message callback for the processor.
    pub fn message_callback(mut self, callback: CallbackFn) -> Self {
        self.message_callback = Some(callback);
        self
    }

    /// Build the [Processor].
    pub fn build(self) -> Result<Processor> {
        self.session_sk
            .session()
            .verify_self()
            .map_err(|e| Error::VerifyError(e.to_string()))?;

        let storage = self
            .storage
            .expect("Please set storage by `storage()` method");

        let mut swarm_builder = SwarmBuilder::new(&self.ice_servers, storage, self.session_sk);

        if let Some(external_address) = self.external_address {
            swarm_builder = swarm_builder.external_address(external_address);
        }

        if let Some(measure) = self.measure {
            swarm_builder = swarm_builder.measure(measure);
        }

        if let Some(callback) = self.message_callback {
            swarm_builder = swarm_builder.message_callback(callback);
        }

        let swarm = Arc::new(swarm_builder.build());
        let stabilization = Arc::new(Stabilization::new(swarm.clone(), self.stabilize_timeout));

        Ok(Processor {
            swarm,
            stabilization,
        })
    }
}

#[cfg(feature = "node")]
impl Metadata for Processor {}

impl Processor {
    /// Listen processor message
    pub fn listen(&self) -> Join<impl Future, impl Future> {
        let swarm = self.swarm.clone();
        let message_listener = async { swarm.listen().await };

        let stb = self.stabilization.clone();
        let stabilization = async { stb.wait().await };

        futures::future::join(message_listener, stabilization)
    }
}

impl Processor {
    /// Get current did
    pub fn did(&self) -> Did {
        self.swarm.did()
    }

    /// Connect peer with remote rings-node jsonrpc server.
    /// * peer_url: the remote rings-node jsonrpc server url.
    pub async fn connect_peer_via_http(&self, peer_url: &str) -> Result<Peer> {
        // request remote offer and sand answer to remote
        tracing::debug!("connect_peer_via_http: {}", peer_url);

        let client = SimpleClient::new(peer_url, None);
        let (_, offer) = self
            .swarm
            .create_offer()
            .await
            .map_err(Error::CreateOffer)?;
        let encoded_offer = offer.encode().map_err(|_| Error::EncodeError)?;
        tracing::debug!("sending encoded offer {:?} to {}", encoded_offer, peer_url);
        let req: serde_json::Value = serde_json::to_value(encoded_offer)
            .map_err(Error::SerdeJsonError)
            .map_err(Error::from)?;

        let resp = client
            .call_method(
                method::Method::AnswerOffer.as_str(),
                jsonrpc_core::Params::Array(vec![req]),
            )
            .await
            .map_err(|e| Error::RemoteRpcError(e.to_string()))?;

        let answer_payload_str: String =
            serde_json::from_value(resp).map_err(|_| Error::EncodeError)?;

        let encoded_answer: Encoded = <Encoded as From<&str>>::from(&answer_payload_str);

        let answer_payload = MessagePayload::<Message>::from_encoded(&encoded_answer)
            .map_err(|_| Error::DecodeError)?;

        let (did, transport) = self
            .swarm
            .accept_answer(answer_payload)
            .await
            .map_err(Error::AcceptAnswer)?;

        Ok(Peer::from((did, transport)))
    }

    /// Connect peer with web3 did.
    /// There are 3 peers: PeerA, PeerB, PeerC.
    /// 1. PeerA has a connection with PeerB.
    /// 2. PeerC has a connection with PeerB.
    /// 3. PeerC can connect PeerA with PeerA's web3 address.
    pub async fn connect_with_did(&self, did: Did, wait_for_open: bool) -> Result<Peer> {
        let transport = self.swarm.connect(did).await.map_err(Error::ConnectError)?;
        tracing::debug!("wait for transport connected");
        if wait_for_open {
            transport
                .wait_for_data_channel_open()
                .await
                .map_err(Error::ConnectError)?;
        }
        Ok(Peer::from((did, transport)))
    }

    /// List all peers.
    pub async fn list_peers(&self) -> Result<Vec<Peer>> {
        let transports = self.swarm.get_transports();
        tracing::debug!(
            "addresses: {:?}",
            transports.iter().map(|(a, _b)| a).collect::<Vec<_>>()
        );
        let data = transports.iter().map(|x| x.into()).collect::<Vec<Peer>>();
        Ok(data)
    }

    /// Get peer by remote did
    pub async fn get_peer(&self, did: Did) -> Result<Peer> {
        let transport = self
            .swarm
            .get_transport(did)
            .ok_or(Error::TransportNotFound)?;
        Ok(Peer::from(&(did, transport)))
    }

    /// Disconnect a peer with web3 did.
    pub async fn disconnect(&self, did: Did) -> Result<()> {
        self.swarm
            .disconnect(did)
            .await
            .map_err(Error::CloseTransportError)
    }

    /// Disconnect all connections.
    pub async fn disconnect_all(&self) {
        let transports = self.swarm.get_transports();

        let close_async = transports
            .iter()
            .map(|(_, t)| t.close())
            .collect::<Vec<_>>();

        futures::future::join_all(close_async).await;
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

    /// Send custom message to a did.
    pub async fn send_message(&self, destination: &str, msg: &[u8]) -> Result<uuid::Uuid> {
        tracing::info!(
            "send_message, destination: {}, text: {:?}",
            destination,
            msg,
        );
        let destination = Did::from_str(destination).map_err(|_| Error::InvalidDid)?;

        let mut new_msg = Vec::with_capacity(msg.len() + 4);
        // chunked mark
        new_msg.push(0);
        new_msg.extend_from_slice(&[0u8; 3]);
        new_msg.extend_from_slice(msg);

        let msg = Message::custom(&new_msg).map_err(Error::SendMessage)?;

        let uuid = self
            .swarm
            .send_message(msg, destination)
            .await
            .map_err(Error::SendMessage)?;
        Ok(uuid)
    }

    /// send http request message to node
    /// - destination: did of destination
    /// - url: ipfs url
    /// - timeout: timeout in millisecond
    #[allow(clippy::too_many_arguments)]
    pub async fn send_http_request_message<U, T>(
        &self,
        destination: &str,
        name: U,
        method: http::Method,
        url: U,
        timeout: T,
        headers: &[(U, U)],
        body: Option<Vec<u8>>,
    ) -> Result<uuid::Uuid>
    where
        U: ToString,
        T: Into<Timeout>,
    {
        let timeout: Timeout = timeout.into();
        tracing::info!(
            "send_http_request_message, destination: {}, url: {:?}, timeout: {:?}",
            destination,
            url.to_string(),
            timeout,
        );
        let msg: BackendMessage = BackendMessage::try_from((
            MessageType::HttpRequest,
            &HttpRequest::new(name, method, url, timeout, headers, body),
        ))?;
        let msg: Vec<u8> = msg.into();

        self.send_message(destination, &msg).await
    }

    /// send simple text message
    /// - destination: did of destination
    /// - text: text message
    pub async fn send_simple_text_message(
        &self,
        destination: &str,
        text: &str,
    ) -> Result<uuid::Uuid> {
        tracing::info!(
            "send_simple_text_message, destination: {}, text: {:?}",
            destination,
            text,
        );

        let msg: BackendMessage =
            BackendMessage::from((MessageType::SimpleText.into(), text.as_bytes()));
        let msg: Vec<u8> = msg.into();
        self.send_message(destination, &msg).await
    }

    /// send custom message
    /// - destination: did of destination
    /// - message_type: custom message type u16
    /// - extra: extra data
    /// - data: payload data
    pub async fn send_custom_message(
        &self,
        destination: &str,
        message_type: u16,
        data: Vec<u8>,
        extra: [u8; 30],
    ) -> Result<uuid::Uuid> {
        tracing::info!(
            "send_custom_message, destination: {}, message_type: {}",
            destination,
            message_type,
        );

        let msg: BackendMessage = BackendMessage::new(message_type, extra, data.as_ref());
        let msg: Vec<u8> = msg.into();
        self.send_message(destination, &msg[..]).await
    }

    /// check local cache of dht
    pub async fn storage_check_cache(&self, did: Did) -> Option<vnode::VirtualNode> {
        self.swarm.storage_check_cache(did).await
    }

    /// fetch virtual node from DHT
    pub async fn storage_fetch(&self, did: Did) -> Result<()> {
        <Swarm as ChordStorageInterface<DATA_REDUNDANT>>::storage_fetch(&self.swarm, did)
            .await
            .map_err(Error::VNodeError)
    }

    /// store virtual node on DHT
    pub async fn storage_store(&self, vnode: vnode::VirtualNode) -> Result<()> {
        <Swarm as ChordStorageInterface<DATA_REDUNDANT>>::storage_store(&self.swarm, vnode)
            .await
            .map_err(Error::VNodeError)
    }

    /// append data to a virtual node on DHT
    pub async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()> {
        <Swarm as ChordStorageInterface<DATA_REDUNDANT>>::storage_append_data(
            &self.swarm,
            topic,
            data,
        )
        .await
        .map_err(Error::VNodeError)
    }

    /// register service
    pub async fn register_service(&self, name: &str) -> Result<()> {
        let encoded_did = self
            .did()
            .to_string()
            .encode()
            .map_err(Error::ServiceRegisterError)?;
        <Swarm as ChordStorageInterface<DATA_REDUNDANT>>::storage_touch_data(
            &self.swarm,
            name,
            encoded_did,
        )
        .await
        .map_err(Error::ServiceRegisterError)
    }

    /// get node info
    pub async fn get_node_info(&self) -> Result<response::NodeInfo> {
        Ok(response::NodeInfo {
            version: crate::util::build_version(),
            swarm: self.swarm.inspect().await,
        })
    }
}

/// Peer struct
#[derive(Clone)]
pub struct Peer {
    /// web3 did of a peer.
    pub did: Token,
    /// transport of the connection.
    pub transport: Arc<Transport>,
}

impl From<(Did, Arc<Transport>)> for Peer {
    fn from((did, transport): (Did, Arc<Transport>)) -> Self {
        Self {
            did: did.into_token(),
            transport,
        }
    }
}

impl From<&(Did, Arc<Transport>)> for Peer {
    fn from((did, transport): &(Did, Arc<Transport>)) -> Self {
        Self {
            did: did.into_token(),
            transport: transport.clone(),
        }
    }
}

impl Peer {
    /// convert peer to response peer
    pub fn into_response_peer(&self, state: Option<String>) -> rings_rpc::response::Peer {
        rings_rpc::response::Peer {
            did: self.did.clone().into_token().to_string(),
            transport_id: self.transport.id.to_string(),
            state: state.unwrap_or_else(|| "Unknown".to_owned()),
        }
    }
}

/// unpack custom message to text
pub fn unpack_text_message(msg: &CustomMessage) -> Result<String> {
    let (left, right) = msg.0.split_at(4);
    if left[0] != 0 {
        return Err(Error::InvalidData);
    }
    let text = String::from_utf8(right.to_vec()).unwrap();
    Ok(text)
}

#[cfg(test)]
#[cfg(feature = "node")]
mod test {
    use futures::lock::Mutex;

    use super::*;
    use crate::prelude::*;
    use crate::tests::native::prepare_processor;

    #[tokio::test]
    async fn test_processor_create_offer() {
        let (processor, path) = prepare_processor(None).await;
        let ti = processor.swarm.create_offer().await.unwrap();
        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 1);
        assert_eq!(pendings.get(0).unwrap().id.to_string(), ti.0.id.to_string());
        tokio::fs::remove_dir_all(path).await.unwrap();
    }

    #[tokio::test]
    async fn test_processor_list_pendings() {
        let (processor, path) = prepare_processor(None).await;
        let ti0 = processor.swarm.create_offer().await.unwrap();
        let ti1 = processor.swarm.create_offer().await.unwrap();
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
        tokio::fs::remove_dir_all(path).await.unwrap();
    }

    #[tokio::test]
    async fn test_processor_close_pending_transport() {
        let (processor, path) = prepare_processor(None).await;
        let ti0 = processor.swarm.create_offer().await.unwrap();
        let _ti1 = processor.swarm.create_offer().await.unwrap();
        let ti2 = processor.swarm.create_offer().await.unwrap();
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
        tokio::fs::remove_dir_all(path).await.unwrap();
    }

    struct MsgCallbackStruct {
        msgs: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl MessageCallback for MsgCallbackStruct {
        async fn custom_message(
            &self,
            _ctx: &MessagePayload<Message>,
            msg: &CustomMessage,
        ) -> Vec<MessageHandlerEvent> {
            let text = unpack_text_message(msg).unwrap();
            let mut msgs = self.msgs.try_lock().unwrap();
            msgs.push(text);
            vec![]
        }

        async fn builtin_message(
            &self,
            _ctx: &MessagePayload<Message>,
        ) -> Vec<MessageHandlerEvent> {
            vec![]
        }
    }

    #[tokio::test]
    async fn test_processor_handshake_msg() {
        let msgs1: Arc<Mutex<Vec<String>>> = Default::default();
        let msgs2: Arc<Mutex<Vec<String>>> = Default::default();
        let callback1 = Box::new(MsgCallbackStruct {
            msgs: msgs1.clone(),
        });
        let callback2 = Box::new(MsgCallbackStruct {
            msgs: msgs2.clone(),
        });

        let (p1, path1) = prepare_processor(Some(callback1)).await;
        let (p2, path2) = prepare_processor(Some(callback2)).await;
        let did1 = p1.did().to_string();
        let did2 = p2.did().to_string();

        println!("p1_did: {}", did1);
        println!("p2_did: {}", did2);

        let swarm1 = p1.swarm.clone();
        let swarm2 = p2.swarm.clone();
        tokio::spawn(async { swarm1.listen().await });
        tokio::spawn(async { swarm2.listen().await });

        let (transport_1, offer) = p1.swarm.create_offer().await.unwrap();

        let pendings_1 = p1.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings_1.len(), 1);
        assert_eq!(
            pendings_1.get(0).unwrap().id.to_string(),
            transport_1.id.to_string()
        );

        let (transport_2, answer) = p2.swarm.answer_offer(offer).await.unwrap();
        let (peer_did, peer_transport) = p1.swarm.accept_answer(answer).await.unwrap();

        assert!(peer_transport.id.eq(&transport_1.id), "transport not same");
        assert!(
            peer_did.to_string().eq(&did2),
            "peer.address got {}, expect: {}",
            peer_did,
            did2
        );
        println!("waiting for connection");
        transport_1
            .connect_success_promise()
            .await
            .unwrap()
            .await
            .unwrap();
        transport_2
            .connect_success_promise()
            .await
            .unwrap()
            .await
            .unwrap();

        assert!(
            transport_1.is_connected().await,
            "transport_1 not connected"
        );
        assert!(
            p1.swarm
                .get_transport(p2.did())
                .unwrap()
                .is_connected()
                .await,
            "p1 transport not connected"
        );
        assert!(
            transport_2.is_connected().await,
            "transport_2 not connected"
        );
        assert!(
            p2.swarm
                .get_transport(p1.did())
                .unwrap()
                .is_connected()
                .await,
            "p2 transport not connected"
        );

        println!("waiting for data channel ready");
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

        let test_text1 = "test1";
        let test_text2 = "test2";

        println!("send_message 1");
        let uuid1 = p1
            .send_message(did2.as_str(), test_text1.as_bytes())
            .await
            .unwrap();
        println!("send_message 1 done, msg id: {}", uuid1);

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        println!("send_message 2");
        let uuid2 = p2
            .send_message(did1.as_str(), test_text2.as_bytes())
            .await
            .unwrap();
        println!("send_message 2 done, msg id: {}", uuid2);

        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

        println!("check received");

        let mut msgs2 = msgs2.try_lock().unwrap();
        let got_msg2 = msgs2.pop().unwrap();
        assert!(
            got_msg2.eq(test_text1),
            "msg received, expect {}, got {}",
            test_text1,
            got_msg2
        );

        let mut msgs1 = msgs1.try_lock().unwrap();
        let got_msg1 = msgs1.pop().unwrap();
        assert!(
            got_msg1.eq(test_text2),
            "msg received, expect {}, got {}",
            test_text2,
            got_msg1
        );
        tokio::fs::remove_dir_all(path1).await.unwrap();
        tokio::fs::remove_dir_all(path2).await.unwrap();
    }
}
