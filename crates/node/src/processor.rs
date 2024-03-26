#![warn(missing_docs)]

//! Processor of rings-node rpc server.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rings_core::dht::Did;
use rings_core::dht::VNodeStorage;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::Message;
use rings_core::prelude::uuid;
use rings_core::storage::MemStorage;
use rings_core::swarm::Swarm;
use rings_core::swarm::SwarmBuilder;
use rings_core::transport::MeasureImpl;
use rings_rpc::protos::rings_node::*;
use serde::Deserialize;
use serde::Serialize;

use crate::backend::types::BackendMessage;
use crate::consts::DATA_REDUNDANT;
use crate::error::Error;
use crate::error::Result;
use crate::measure::PeriodicMeasure;
use crate::prelude::vnode;
use crate::prelude::wasm_export;
use crate::prelude::ChordStorageInterface;
use crate::prelude::ChordStorageInterfaceCacheChecker;
use crate::prelude::SessionSk;

/// ProcessorConfig is usually serialized as json or yaml.
/// There is a `from_config` method in [ProcessorBuilder] used to initialize the Builder with a serialized ProcessorConfig.
#[derive(Clone, Debug)]
#[wasm_export]
pub struct ProcessorConfig {
    /// ICE servers for webrtc
    ice_servers: String,
    /// External address for webrtc
    external_address: Option<String>,
    /// [SessionSk].
    session_sk: SessionSk,
    /// Stabilization interval.
    stabilize_interval: Duration,
}

#[wasm_export]
impl ProcessorConfig {
    /// Creates a new `ProcessorConfig` instance without an external address.
    pub fn new(ice_servers: String, session_sk: SessionSk, stabilize_interval: u64) -> Self {
        Self {
            ice_servers,
            external_address: None,
            session_sk,
            stabilize_interval: Duration::from_secs(stabilize_interval),
        }
    }

    /// Creates a new `ProcessorConfig` instance with an external address.
    pub fn new_with_ext_addr(
        ice_servers: String,
        session_sk: SessionSk,
        stabilize_interval: u64,
        external_address: String,
    ) -> Self {
        Self {
            ice_servers,
            external_address: Some(external_address),
            session_sk,
            stabilize_interval: Duration::from_secs(stabilize_interval),
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
    /// A string representing ICE servers for WebRTC
    ice_servers: String,
    /// An optional string representing the external address for WebRTC
    external_address: Option<String>,
    /// A string representing the dumped `SessionSk`.
    session_sk: String,
    /// An unsigned integer representing the stabilization interval in seconds.
    stabilize_interval: u64,
}

impl ProcessorConfigSerialized {
    /// Creates a new `ProcessorConfigSerialized` instance without an external address.
    pub fn new(ice_servers: String, session_sk: String, stabilize_interval: u64) -> Self {
        Self {
            ice_servers,
            external_address: None,
            session_sk,
            stabilize_interval,
        }
    }

    /// Creates a new `ProcessorConfigSerialized` instance with an external address.
    pub fn new_with_ext_addr(
        ice_servers: String,
        session_sk: String,
        stabilize_interval: u64,
        external_address: String,
    ) -> Self {
        Self {
            ice_servers,
            external_address: Some(external_address),
            session_sk,
            stabilize_interval,
        }
    }
}

impl TryFrom<ProcessorConfig> for ProcessorConfigSerialized {
    type Error = Error;
    fn try_from(ins: ProcessorConfig) -> Result<Self> {
        Ok(Self {
            ice_servers: ins.ice_servers.clone(),
            external_address: ins.external_address.clone(),
            session_sk: ins.session_sk.dump()?,
            stabilize_interval: ins.stabilize_interval.as_secs(),
        })
    }
}

impl TryFrom<ProcessorConfigSerialized> for ProcessorConfig {
    type Error = Error;
    fn try_from(ins: ProcessorConfigSerialized) -> Result<Self> {
        Ok(Self {
            ice_servers: ins.ice_servers.clone(),
            external_address: ins.external_address.clone(),
            session_sk: SessionSk::from_str(&ins.session_sk)?,
            stabilize_interval: Duration::from_secs(ins.stabilize_interval),
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
    storage: Option<VNodeStorage>,
    measure: Option<MeasureImpl>,
    stabilize_interval: Duration,
}

/// Processor for rings-node rpc server
#[derive(Clone)]
pub struct Processor {
    /// a swarm instance
    pub swarm: Arc<Swarm>,
    stabilize_interval: Duration,
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
            stabilize_interval: config.stabilize_interval,
        })
    }

    /// Set the storage for the processor.
    pub fn storage(mut self, storage: VNodeStorage) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Set the measure for the processor.
    pub fn measure(mut self, implement: PeriodicMeasure) -> Self {
        self.measure = Some(Box::new(implement));
        self
    }

    /// Build the [Processor].
    pub fn build(self) -> Result<Processor> {
        self.session_sk
            .session()
            .verify_self()
            .map_err(|e| Error::VerifyError(e.to_string()))?;

        let storage = self.storage.unwrap_or_else(|| Box::new(MemStorage::new()));

        let mut swarm_builder = SwarmBuilder::new(&self.ice_servers, storage, self.session_sk);

        if let Some(external_address) = self.external_address {
            swarm_builder = swarm_builder.external_address(external_address);
        }

        if let Some(measure) = self.measure {
            swarm_builder = swarm_builder.measure(measure);
        }
        let swarm = Arc::new(swarm_builder.build());

        Ok(Processor {
            swarm,
            stabilize_interval: self.stabilize_interval,
        })
    }
}

impl Processor {
    /// Get current did
    pub fn did(&self) -> Did {
        self.swarm.did()
    }

    /// Run stabilization daemon
    pub async fn listen(&self) {
        let stabilizer = self.swarm.stabilizer();
        Arc::new(stabilizer).wait(self.stabilize_interval).await
    }

    /// Connect peer with web3 did.
    /// There are 3 peers: PeerA, PeerB, PeerC.
    /// 1. PeerA has a connection with PeerB.
    /// 2. PeerC has a connection with PeerB.
    /// 3. PeerC can connect PeerA with PeerA's web3 address.
    pub async fn connect_with_did(&self, did: Did) -> Result<()> {
        self.swarm.connect(did).await.map_err(Error::ConnectError)?;
        Ok(())
    }

    /// Disconnect a peer with web3 did.
    pub async fn disconnect(&self, did: Did) -> Result<()> {
        self.swarm
            .disconnect(did)
            .await
            .map_err(Error::CloseConnectionError)
    }

    /// Send custom message to a did.
    pub async fn send_message(&self, destination: Did, msg: &[u8]) -> Result<uuid::Uuid> {
        tracing::info!(
            "send_message, destination: {}, message size: {:?}",
            destination,
            msg.len(),
        );

        let msg = Message::custom(msg).map_err(Error::SendMessage)?;

        self.swarm
            .send_message(msg, destination)
            .await
            .map_err(Error::SendMessage)
    }

    /// Send custom message to a did.
    pub async fn send_backend_message(
        &self,
        destination: Did,
        backend_msg: BackendMessage,
    ) -> Result<uuid::Uuid> {
        let msg_bytes = bincode::serialize(&backend_msg).map_err(|_| Error::EncodeError)?;
        self.send_message(destination, &msg_bytes).await
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
    pub async fn get_node_info(&self) -> Result<NodeInfoResponse> {
        Ok(NodeInfoResponse {
            version: crate::util::build_version(),
            swarm: Some(self.swarm.inspect().await.into()),
        })
    }
}

#[cfg(test)]
#[cfg(feature = "node")]
mod test {
    use futures::lock::Mutex;
    use rings_core::swarm::callback::SwarmCallback;
    use rings_core::swarm::ConnectionHandshake;
    use rings_transport::core::transport::WebrtcConnectionState;

    use super::*;
    use crate::prelude::*;
    use crate::tests::native::prepare_processor;

    #[tokio::test]
    async fn test_processor_create_offer() {
        let peer_did = SecretKey::random().address().into();
        let processor = prepare_processor().await;
        processor.swarm.create_offer(peer_did).await.unwrap();
        let conn_dids = processor.swarm.transport.get_connection_ids();
        assert_eq!(conn_dids.len(), 1);
        assert_eq!(conn_dids.first().unwrap(), &peer_did);
    }

    struct SwarmCallbackInstance {
        pub msgs: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl SwarmCallback for SwarmCallbackInstance {
        async fn on_inbound(
            &self,
            payload: &MessagePayload,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            let msg: Message = payload.transaction.data().map_err(Box::new)?;

            if let Message::CustomMessage(ref msg) = msg {
                let text = String::from_utf8(msg.0.to_vec()).unwrap();
                let mut msgs = self.msgs.try_lock().unwrap();
                msgs.push(text);
            }

            Ok(())
        }
    }

    #[tokio::test]
    async fn test_processor_handshake_msg() {
        let callback1 = Arc::new(SwarmCallbackInstance {
            msgs: Mutex::new(Vec::new()),
        });
        let callback2 = Arc::new(SwarmCallbackInstance {
            msgs: Mutex::new(Vec::new()),
        });

        let p1 = prepare_processor().await;
        let p2 = prepare_processor().await;

        p1.swarm.set_callback(callback1.clone()).unwrap();
        p2.swarm.set_callback(callback2.clone()).unwrap();

        let did1 = p1.did();
        let did2 = p2.did();

        println!("p1_did: {}", did1);
        println!("p2_did: {}", did2);

        let offer = p1.swarm.create_offer(p2.did()).await.unwrap();
        assert_eq!(
            p1.swarm
                .transport
                .get_connection(p2.did())
                .unwrap()
                .webrtc_connection_state(),
            WebrtcConnectionState::New,
        );

        let answer = p2.swarm.answer_offer(offer).await.unwrap();
        p1.swarm.accept_answer(answer).await.unwrap();

        println!("waiting for connection");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        assert_eq!(
            p1.swarm
                .transport
                .get_connection(p2.did())
                .unwrap()
                .webrtc_connection_state(),
            WebrtcConnectionState::Connected,
            "p1 connection not connected"
        );
        assert_eq!(
            p2.swarm
                .transport
                .get_connection(p1.did())
                .unwrap()
                .webrtc_connection_state(),
            WebrtcConnectionState::Connected,
            "p2 connection not connected"
        );

        let test_text1 = "test1";
        let test_text2 = "test2";

        println!("send_message 1");
        let uuid1 = p1.send_message(did2, test_text1.as_bytes()).await.unwrap();
        println!("send_message 1 done, msg id: {}", uuid1);

        println!("send_message 2");
        let uuid2 = p2.send_message(did1, test_text2.as_bytes()).await.unwrap();
        println!("send_message 2 done, msg id: {}", uuid2);

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        println!("check received");

        let mut msgs2 = callback2.msgs.try_lock().unwrap();
        let got_msg2 = msgs2.pop().unwrap();
        assert!(
            got_msg2.eq(test_text1),
            "msg received, expect {}, got {}",
            test_text1,
            got_msg2
        );

        let mut msgs1 = callback1.msgs.try_lock().unwrap();
        let got_msg1 = msgs1.pop().unwrap();
        assert!(
            got_msg1.eq(test_text2),
            "msg received, expect {}, got {}",
            test_text2,
            got_msg1
        );
    }
}
