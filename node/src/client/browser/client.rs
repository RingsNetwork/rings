#![warn(missing_docs)]
//! Browser Client implementation
#![allow(non_snake_case, non_upper_case_globals, clippy::ptr_offset_with_cast)]
use std::convert::TryFrom;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::anyhow;
use arrayref::array_refs;
use bytes::Bytes;
use js_sys;
use js_sys::Uint8Array;
use rings_core::async_trait;
use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::message::CustomMessage;
use rings_core::message::MessagePayload;
use rings_core::prelude::vnode;
use rings_core::prelude::vnode::VirtualNode;
use rings_core::swarm::callback::SwarmCallback;
use rings_core::utils::js_utils;
use rings_core::utils::js_value;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::WebrtcConnectionState;
use serde::Deserialize;
use serde::Serialize;
use wasm_bindgen;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures;
use wasm_bindgen_futures::future_to_promise;
use wasm_bindgen_futures::JsFuture;

use crate::backend::types::BackendMessage;
use crate::backend::types::HttpResponse;
use crate::backend::types::MessageType;
use crate::client::AsyncSigner;
use crate::client::Client;
use crate::client::Signer;
use crate::consts::BACKEND_MTU;
use crate::prelude::chunk::Chunk;
use crate::prelude::chunk::ChunkList;
use crate::prelude::chunk::ChunkManager;
use crate::prelude::http;
use crate::prelude::message;
use crate::prelude::wasm_export;
use crate::processor::ProcessorConfig;

/// AddressType enum contains `DEFAULT` and `ED25519`.
#[wasm_export]
pub enum AddressType {
    /// Default address type, hex string of sha1(pubkey)
    DEFAULT,
    /// Ed25519 style address type, hex string of pubkey
    Ed25519,
}

#[wasm_export]
impl Client {
    /// Create new instance of Client, return Promise
    /// Ice_servers should obey forrmat: "[turn|strun]://<Address>:<Port>;..."
    /// Account is hex string
    /// Account should format as same as account_type declared
    /// Account_type is lowercase string, possible input are: `eip191`, `ed25519`, `bip137`, for more imformation,
    /// please check [rings_core::ecc]
    /// Signer should be `async function (proof: string): Promise<Unit8Array>`
    /// Signer should function as same as account_type declared, Eg: eip191 or secp256k1 or ed25519.
    #[wasm_bindgen(constructor)]
    pub fn new_instance(
        ice_servers: String,
        stabilize_timeout: usize,
        account: String,
        account_type: String,
        signer: js_sys::Function,
        callback: Option<MessageCallbackInstance>,
    ) -> js_sys::Promise {
        fn wrapped_signer(signer: js_sys::Function) -> AsyncSigner {
            Box::new(
                move |data: String| -> Pin<Box<dyn Future<Output = Vec<u8>>>> {
                    let signer = signer.clone();
                    Box::pin(async move {
                        let signer = signer.clone();
                        let sig: js_sys::Uint8Array = Uint8Array::from(
                            JsFuture::from(js_sys::Promise::from(
                                signer
                                    .call1(&JsValue::NULL, &JsValue::from_str(&data))
                                    .expect("Failed on call external Js Function"),
                            ))
                            .await
                            .expect("Failed await call external Js Promise"),
                        );
                        sig.to_vec()
                    })
                },
            )
        }

        future_to_promise(async move {
            let signer = wrapped_signer(signer);
            let client = Client::new_client_internal(
                ice_servers,
                stabilize_timeout,
                account,
                account_type,
                Signer::Async(Box::new(signer)),
            )
            .await?;
            if let Some(cb) = callback {
                client
                    .set_swarm_callback(Arc::new(cb))
                    .expect("Failed on set swarm callback");
            }
            Ok(JsValue::from(client))
        })
    }

    /// Create new client instance with serialized config (yaml/json)
    pub fn new_client_with_serialized_config(
        config: String,
        callback: Option<MessageCallbackInstance>,
    ) -> js_sys::Promise {
        let cfg: ProcessorConfig = serde_yaml::from_str(&config).unwrap();
        Self::new_client_with_config(cfg, callback)
    }

    /// Create a new client instance.
    pub fn new_client_with_config(
        config: ProcessorConfig,
        callback: Option<MessageCallbackInstance>,
    ) -> js_sys::Promise {
        Self::new_client_with_storage(config, callback, "rings-node".to_string())
    }

    /// get self web3 address
    #[wasm_bindgen(getter)]
    pub fn address(&self) -> String {
        self.processor.did().to_string()
    }

    ///  create new unsigned Client
    pub fn new_client_with_storage(
        config: ProcessorConfig,
        callback: Option<MessageCallbackInstance>,
        storage_name: String,
    ) -> js_sys::Promise {
        future_to_promise(async move {
            let client = Self::new_client_with_storage_internal(config, storage_name)
                .await
                .map_err(JsError::from)?;
            if let Some(cb) = callback {
                client
                    .set_swarm_callback(Arc::new(cb))
                    .expect("Failed on set swarm callback");
            }
            Ok(JsValue::from(client))
        })
    }

    /// Request local rpc interface
    pub fn request(
        &self,
        method: String,
        params: JsValue,
        opt_id: Option<String>,
    ) -> js_sys::Promise {
        let ins = self.clone();
        future_to_promise(async move {
            let params = super::utils::parse_params(params)
                .map_err(|e| JsError::new(e.to_string().as_str()))?;
            let ret = ins
                .request_internal(method, params, opt_id)
                .await
                .map_err(JsError::from)?;
            Ok(js_value::serialize(&ret).map_err(JsError::from)?)
        })
    }

    /// listen message callback.
    pub fn listen(&self) -> js_sys::Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            p.listen().await;
            Ok(JsValue::null())
        })
    }

    /// connect peer with remote jsonrpc-server url
    pub fn connect_peer_via_http(&self, remote_url: String) -> js_sys::Promise {
        log::debug!("remote_url: {}", remote_url);
        let p = self.processor.clone();
        future_to_promise(async move {
            let peer = p
                .connect_peer_via_http(remote_url.as_str())
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from_str(peer.did.as_str()))
        })
    }

    /// connect peer with web3 address, without waiting for connection channel connected
    pub fn connect_with_address_without_wait(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            let peer = p
                .connect_with_did(did, false)
                .await
                .map_err(JsError::from)?;
            let state = peer.connection.webrtc_connection_state();

            Ok(JsValue::try_from(&Peer::from((state, peer.did)))?)
        })
    }

    /// connect peer with web3 address, and wait for connection channel connected
    /// example:
    /// ```typescript
    /// const client1 = new Client()
    /// const client2 = new Client()
    /// const client3 = new Client()
    /// await create_connection(client1, client2);
    /// await create_connection(client2, client3);
    /// await client1.connect_with_did(client3.address())
    /// ```
    pub fn connect_with_address(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            let peer = p.connect_with_did(did, true).await.map_err(JsError::from)?;
            let state = peer.connection.webrtc_connection_state();
            Ok(JsValue::try_from(&Peer::from((state, peer.did)))?)
        })
    }

    /// list all connect peers
    pub fn list_peers(&self) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let peers = p.list_peers().await.map_err(JsError::from)?;
            let mut js_array = js_sys::Array::new();
            js_array.extend(peers.iter().flat_map(|x| {
                JsValue::try_from(&Peer::from((
                    x.connection.webrtc_connection_state(),
                    x.did.clone(),
                )))
            }));
            Ok(js_array.into())
        })
    }

    /// get info for self, will return build version and inspection of swarm
    pub fn get_node_info(&self) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let info = p.get_node_info().await.map_err(JsError::from)?;
            let v = js_value::serialize(&info).map_err(JsError::from)?;
            Ok(v)
        })
    }

    /// disconnect a peer with web3 address
    pub fn disconnect(&self, address: String, addr_type: Option<AddressType>) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            p.disconnect(did).await.map_err(JsError::from)?;

            Ok(JsValue::from_str(did.to_string().as_str()))
        })
    }

    /// disconnect all connected nodes
    pub fn disconnect_all(&self) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            p.disconnect_all().await;
            Ok(JsValue::from_str("ok"))
        })
    }

    /// send custom message to peer.
    pub fn send_message(&self, destination: String, msg: js_sys::Uint8Array) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            p.send_message(destination.as_str(), &msg.to_vec())
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from_bool(true))
        })
    }

    /// get peer by address
    pub fn get_peer(&self, address: String, addr_type: Option<AddressType>) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            let peer = p.get_peer(did).await.map_err(JsError::from)?;
            let state = peer.connection.webrtc_connection_state();
            Ok(JsValue::try_from(&Peer::from((state, peer.did)))?)
        })
    }

    /// get connection state by address
    pub fn connection_state(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            let conn = p
                .swarm
                .get_connection(did)
                .ok_or_else(|| JsError::new("connection not found"))?;
            let state = format!("{:?}", conn.webrtc_connection_state());
            Ok(js_value::serialize(&state).map_err(JsError::from)?)
        })
    }

    /// wait for connection connected
    /// * address: peer's address
    pub fn wait_for_connected(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            let peer = p.get_peer(did).await.map_err(JsError::from)?;
            log::debug!("wait_for_data_channel_open start");
            if let Err(e) = peer.connection.webrtc_wait_for_data_channel_open().await {
                log::warn!("wait_for_data_channel_open failed: {}", e);
            }
            log::debug!("wait_for_data_channel_open done");
            Ok(JsValue::null())
        })
    }

    /// wait for data channel open
    ///   * address: peer's address
    pub fn wait_for_data_channel_open(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            let peer = p.get_peer(did).await.map_err(JsError::from)?;
            log::debug!("wait_for_data_channel_open start");
            if let Err(e) = peer.connection.webrtc_wait_for_data_channel_open().await {
                log::warn!("wait_for_data_channel_open failed: {}", e);
            }
            log::debug!("wait_for_data_channel_open done");
            Ok(JsValue::null())
        })
    }

    /// Check local cache
    pub fn storage_check_cache(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            let v_node = p.storage_check_cache(did).await;
            if let Some(v) = v_node {
                let data = js_value::serialize(&v).map_err(JsError::from)?;
                Ok(data)
            } else {
                Ok(JsValue::null())
            }
        })
    }

    /// fetch storage with given did
    pub fn storage_fetch(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            p.storage_fetch(did).await.map_err(JsError::from)?;
            Ok(JsValue::null())
        })
    }

    /// store virtual node on DHT
    pub fn storage_store(&self, data: String) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let vnode_info = vnode::VirtualNode::try_from(data).map_err(JsError::from)?;
            p.storage_store(vnode_info).await.map_err(JsError::from)?;
            Ok(JsValue::null())
        })
    }

    /// send http request message to remote
    /// - destination: did
    /// - name: service name
    /// - method: http method
    /// - url: http url like `/ipfs/abc1234` `/ipns/abc`
    /// - timeout: timeout in milliseconds
    /// - headers: headers of request
    /// - body: body of request
    #[allow(clippy::too_many_arguments)]
    pub fn send_http_request(
        &self,
        destination: String,
        name: String,
        method: String,
        url: String,
        timeout: u64,
        headers: JsValue,
        body: Option<js_sys::Uint8Array>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            let method = http::Method::from_str(method.as_str()).map_err(JsError::from)?;

            let headers: Vec<(String, String)> = if headers.is_null() {
                Vec::new()
            } else if headers.is_object() {
                let mut header_vec: Vec<(String, String)> = Vec::new();
                let obj = js_sys::Object::from(headers);
                let entries = js_sys::Object::entries(&obj);
                for e in entries.iter() {
                    if js_sys::Array::is_array(&e) {
                        let arr = js_sys::Array::from(&e);
                        if arr.length() != 2 {
                            continue;
                        }
                        let k = arr.get(0).as_string().unwrap();
                        let v = arr.get(1);
                        if v.is_string() {
                            let v = v.as_string().unwrap();
                            header_vec.push((k, v))
                        }
                    }
                }
                header_vec
            } else {
                Vec::new()
            };

            let b = body.map(|item| item.to_vec());

            let tx_id = p
                .send_http_request_message(
                    destination.as_str(),
                    name.as_str(),
                    method,
                    url.as_str(),
                    timeout,
                    &headers
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect::<Vec<(&str, &str)>>(),
                    b,
                )
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from_str(tx_id.to_string().as_str()))
        })
    }

    /// send simple text message to remote
    /// - destination: A did of destination
    /// - text: text message
    pub fn send_simple_text_message(&self, destination: String, text: String) -> js_sys::Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            let tx_id = p
                .send_simple_text_message(destination.as_str(), text.as_str())
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from_str(tx_id.to_string().as_str()))
        })
    }

    /// send custom message to remote
    /// - destination: A did of destination
    /// - message_type: u16
    /// - data: uint8Array
    pub fn send_custom_message(
        &self,
        destination: String,
        message_type: u16,
        data: js_sys::Uint8Array,
    ) -> js_sys::Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            let tx_id = p
                .send_custom_message(destination.as_str(), message_type, data.to_vec(), [0u8; 30])
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from_str(tx_id.to_string().as_str()))
        })
    }

    /// lookup service did on DHT by its name
    /// - name: The name of service
    pub fn lookup_service(&self, name: String) -> js_sys::Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            let rid = VirtualNode::gen_did(&name).map_err(JsError::from)?;

            tracing::debug!("browser lookup_service storage_fetch: {}", rid);
            p.storage_fetch(rid).await.map_err(JsError::from)?;
            tracing::debug!("browser lookup_service finish storage_fetch: {}", rid);
            js_utils::window_sleep(500).await?;
            let result = p.storage_check_cache(rid).await;

            if let Some(vnode) = result {
                let dids = vnode
                    .data
                    .iter()
                    .map(|v| v.decode())
                    .filter_map(|v| v.ok())
                    .map(|x: String| JsValue::from_str(x.as_str()))
                    .collect::<js_sys::Array>();
                Ok(JsValue::from(dids))
            } else {
                Ok(JsValue::from(js_sys::Array::new()))
            }
        })
    }
}

/// MessageCallback instance for Browser
#[wasm_export]
pub struct MessageCallbackInstance {
    custom_message: Arc<js_sys::Function>,
    http_response_message: Arc<js_sys::Function>,
    builtin_message: Arc<js_sys::Function>,
    chunk_list: Arc<Mutex<ChunkList<BACKEND_MTU>>>,
}

#[wasm_export]
impl MessageCallbackInstance {
    /// Create a new instance of message callback, this function accept three args:
    ///
    /// * custom_message: function(from: string, message: string) -> ();
    /// * http_response_message: function(from: string, message: string) -> ();
    /// * builtin_message: function(from: string, message: string) -> ();
    #[wasm_bindgen(constructor)]
    pub fn new(
        custom_message: &js_sys::Function,
        http_response_message: &js_sys::Function,
        builtin_message: &js_sys::Function,
    ) -> Result<MessageCallbackInstance, JsError> {
        Ok(MessageCallbackInstance {
            custom_message: Arc::new(custom_message.clone()),
            http_response_message: Arc::new(http_response_message.clone()),
            builtin_message: Arc::new(builtin_message.clone()),
            chunk_list: Default::default(),
        })
    }
}

impl MessageCallbackInstance {
    pub(crate) async fn handle_message_data(
        &self,
        relay: &MessagePayload,
        data: &Bytes,
    ) -> anyhow::Result<()> {
        let m = BackendMessage::try_from(data.to_vec()).map_err(|e| anyhow::anyhow!("{}", e))?;
        match m.message_type.into() {
            MessageType::SimpleText => {
                self.handle_simple_text_message(relay, m.data.as_slice())
                    .await?;
            }
            MessageType::HttpResponse => {
                self.handle_http_response(relay, m.data.as_slice()).await?;
            }
            _ => {
                return Ok(());
            }
        };
        Ok(())
    }

    pub(crate) async fn handle_simple_text_message(
        &self,
        relay: &MessagePayload,
        data: &[u8],
    ) -> anyhow::Result<()> {
        log::debug!("custom_message received: {:?}", data);
        let msg_content = js_sys::Uint8Array::from(data);

        let this = JsValue::null();
        if let Ok(r) =
            self.custom_message
                .call2(&this, &js_value::serialize(&relay).unwrap(), &msg_content)
        {
            if let Ok(p) = js_sys::Promise::try_from(r) {
                if let Err(e) = wasm_bindgen_futures::JsFuture::from(p).await {
                    log::warn!("invoke on_custom_message error: {:?}", e);
                    return Err(anyhow::anyhow!("{:?}", e));
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn handle_http_response(
        &self,
        payload: &MessagePayload,
        data: &[u8],
    ) -> anyhow::Result<()> {
        let msg_content = data;
        log::info!(
            "message of {:?} received, before gunzip: {:?}",
            payload.transaction.tx_id,
            msg_content.len(),
        );
        let this = JsValue::null();
        let msg_content = message::decode_gzip_data(&Bytes::from(data.to_vec())).unwrap();
        log::info!(
            "message of {:?} received, after gunzip: {:?}",
            payload.transaction.tx_id,
            msg_content.len(),
        );
        let http_response: HttpResponse = bincode::deserialize(&msg_content)?;
        let msg_content = js_value::serialize(&http_response)
            .map_err(|_| anyhow!("Failed on serialize message"))?;
        if let Ok(r) = self.http_response_message.call2(
            &this,
            &js_value::serialize(&payload).map_err(|_| anyhow!("Failed on serialize message"))?,
            &msg_content,
        ) {
            if let Ok(p) = js_sys::Promise::try_from(r) {
                if let Err(e) = wasm_bindgen_futures::JsFuture::from(p).await {
                    log::warn!("invoke on_custom_message error: {:?}", e);
                }
            }
        };
        Ok(())
    }

    fn handle_chunk_data(&self, data: &[u8]) -> anyhow::Result<Option<Bytes>> {
        let c_lock = self.chunk_list.try_lock();
        if c_lock.is_err() {
            return Err(anyhow!("lock chunklist failed"));
        }
        let mut chunk_list = c_lock.unwrap();

        let chunk_item =
            Chunk::from_bincode(data).map_err(|_| anyhow!("BincodeDeserialize failed"))?;

        log::debug!(
            "before handle chunk, chunk list len: {}",
            chunk_list.as_vec().len()
        );
        log::debug!(
            "chunk id: {}, total size: {}",
            chunk_item.meta.id,
            chunk_item.chunk[1]
        );
        let data = chunk_list.handle(chunk_item);
        log::debug!(
            "after handle chunk, chunk list len: {}",
            chunk_list.as_vec().len()
        );

        Ok(data)
    }

    async fn handle_custom_message(&self, payload: &MessagePayload, msg: &CustomMessage) {
        if msg.0.len() < 2 {
            return;
        }

        let (left, right) = array_refs![&msg.0, 4; ..;];
        let (&[tag], _) = array_refs![left, 1, 3];

        let data = if tag == 1 {
            let data = self.handle_chunk_data(right);
            if let Err(e) = data {
                log::error!("handle chunk data failed: {}", e);
                return;
            }
            let data = data.unwrap();
            log::debug!("chunk message of {:?} received", payload.transaction.tx_id);
            if let Some(data) = data {
                data
            } else {
                log::info!(
                    "chunk message of {:?} not complete",
                    payload.transaction.tx_id
                );
                return;
            }
        } else if tag == 0 {
            Bytes::from(right.to_vec())
        } else {
            log::error!("invalid message tag: {}", tag);
            return;
        };
        if let Err(e) = self.handle_message_data(payload, &data).await {
            log::error!("handle http_server_msg failed, {}", e);
        }
    }

    async fn handle_builtin_message(&self, payload: &MessagePayload) {
        let this = JsValue::null();
        if let Ok(r) = self
            .builtin_message
            .call1(&this, &js_value::serialize(&payload).unwrap())
        {
            if let Ok(p) = js_sys::Promise::try_from(r) {
                if let Err(e) = wasm_bindgen_futures::JsFuture::from(p).await {
                    log::warn!("invoke on_builtin_message error: {:?}", e);
                }
            }
        }
    }
}

#[async_trait(?Send)]
impl SwarmCallback for MessageCallbackInstance {
    async fn on_inbound(&self, payload: &MessagePayload) -> Result<(), Box<dyn std::error::Error>> {
        let msg: message::Message = payload.transaction.data().map_err(Box::new)?;

        match msg {
            message::Message::CustomMessage(ref msg) => {
                self.handle_custom_message(payload, msg).await
            }
            _ => self.handle_builtin_message(payload).await,
        }

        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Peer {
    pub address: String,
    pub state: String,
}

impl From<(WebrtcConnectionState, String)> for Peer {
    fn from((st, address): (WebrtcConnectionState, String)) -> Self {
        Self {
            address,
            state: format!("{:?}", st),
        }
    }
}

impl TryFrom<&Peer> for JsValue {
    type Error = JsError;

    fn try_from(value: &Peer) -> Result<Self, Self::Error> {
        js_value::serialize(value).map_err(JsError::from)
    }
}

#[wasm_export]
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
/// Internal Info struct
pub struct InternalInfo {
    build_version: String,
}

#[wasm_export]
impl InternalInfo {
    /// Get InternalInfo
    fn build() -> Self {
        Self {
            build_version: crate::util::build_version(),
        }
    }

    /// build_version getter
    #[wasm_bindgen(getter)]
    pub fn build_version(&self) -> String {
        self.build_version.clone()
    }
}

/// Build InternalInfo
#[wasm_export]
pub fn internal_info() -> InternalInfo {
    InternalInfo::build()
}

fn get_did(address: &str, addr_type: AddressType) -> Result<Did, JsError> {
    let did = match addr_type {
        AddressType::DEFAULT => {
            Did::from_str(address).map_err(|_| JsError::new("invalid address"))?
        }
        AddressType::Ed25519 => PublicKey::try_from_b58t(address)
            .map_err(|_| JsError::new("invalid address"))?
            .address()
            .into(),
    };
    Ok(did)
}

/// Get address from hex pubkey
///  * pubkey: hex pubkey
#[wasm_export]
pub fn get_address_from_hex_pubkey(pubkey: String) -> Result<String, JsError> {
    Ok(Did::from(
        PublicKey::from_hex_string(pubkey.as_str())
            .map_err(JsError::from)?
            .address(),
    )
    .to_string())
}

/// Get address from other address
///   * address: source address
///   * addr_type: source address type
#[wasm_export]
pub fn get_address(address: &str, addr_type: AddressType) -> Result<String, JsError> {
    Ok(get_did(address, addr_type)?.to_string())
}
