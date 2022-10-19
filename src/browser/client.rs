#![allow(non_snake_case, non_upper_case_globals)]
use std::collections::HashMap;
use std::convert::TryFrom;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

use serde::Deserialize;
use serde::Serialize;

use crate::backend_client::BackendMessage;
use crate::backend_client::HttpServerMessage;
use crate::backend_client::HttpServerRequest;
use crate::jsonrpc::RpcMeta;
use crate::prelude::chunk;
use crate::prelude::chunk::ChunkManager;
use crate::prelude::js_sys;
use crate::prelude::rings_core::async_trait;
use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_core::dht::Stabilization;
use crate::prelude::rings_core::dht::TStabilize;
use crate::prelude::rings_core::ecc::PublicKey;
use crate::prelude::rings_core::ecc::SecretKey;
use crate::prelude::rings_core::message::CustomMessage;
use crate::prelude::rings_core::message::Encoded;
use crate::prelude::rings_core::message::MaybeEncrypted;
use crate::prelude::rings_core::message::Message;
use crate::prelude::rings_core::message::MessageCallback;
use crate::prelude::rings_core::message::MessageHandler;
use crate::prelude::rings_core::message::MessagePayload;
use crate::prelude::rings_core::prelude::uuid::Uuid;
use crate::prelude::rings_core::prelude::vnode;
use crate::prelude::rings_core::prelude::web3::ethabi::Token;
use crate::prelude::rings_core::session::AuthorizedInfo;
use crate::prelude::rings_core::session::SessionManager;
use crate::prelude::rings_core::session::Signer;
use crate::prelude::rings_core::storage::PersistenceStorage;
use crate::prelude::rings_core::swarm::SwarmBuilder;
use crate::prelude::rings_core::transports::manager::TransportManager;
use crate::prelude::rings_core::transports::Transport;
use crate::prelude::rings_core::types::ice_transport::IceTransportInterface;
use crate::prelude::rings_core::types::message::MessageListener;
use crate::prelude::wasm_bindgen;
use crate::prelude::wasm_bindgen::prelude::*;
use crate::prelude::wasm_bindgen_futures;
use crate::prelude::wasm_bindgen_futures::future_to_promise;
use crate::prelude::web3::contract::tokens::Tokenizable;
use crate::prelude::web_sys::RtcIceConnectionState;
use crate::processor::Processor;
use crate::util::from_rtc_ice_connection_state;

#[wasm_bindgen]
pub enum SignerMode {
    DEFAULT,
    EIP712,
}

impl From<SignerMode> for Signer {
    fn from(v: SignerMode) -> Self {
        match v {
            SignerMode::DEFAULT => Self::DEFAULT,
            SignerMode::EIP712 => Self::EIP712,
        }
    }
}

#[wasm_bindgen]
pub enum AddressType {
    DEFAULT,
    ED25519,
}

impl ToString for AddressType {
    fn to_string(&self) -> String {
        match self {
            Self::DEFAULT => "default".to_owned(),
            Self::ED25519 => "ED25519".to_owned(),
        }
    }
}

#[wasm_bindgen]
#[derive(Clone)]
pub struct UnsignedInfo {
    key_addr: Did,
    auth: AuthorizedInfo,
    random_key: SecretKey,
}

#[wasm_bindgen]
impl UnsignedInfo {
    /// Create a new `UnsignedInfo` instance with SignerMode::EIP712
    #[wasm_bindgen(constructor)]
    pub fn new(key_addr: String) -> Result<UnsignedInfo, wasm_bindgen::JsError> {
        Self::new_with_signer(key_addr, SignerMode::EIP712)
    }

    /// Create a new `UnsignedInfo` instance
    ///   * key_addr: wallet address
    ///   * signer: `SignerMode`
    pub fn new_with_signer(key_addr: String, signer: SignerMode) -> Result<UnsignedInfo, JsError> {
        let key_addr = Did::from_str(key_addr.as_str())?;
        let (auth, random_key) =
            SessionManager::gen_unsign_info(key_addr, None, Some(signer.into()));

        Ok(UnsignedInfo {
            auth,
            random_key,
            key_addr,
        })
    }

    /// Create a new `UnsignedInfo` instance
    ///   * pubkey: solana wallet pubkey
    pub fn new_with_address(
        address: String,
        addr_type: AddressType,
    ) -> Result<UnsignedInfo, JsError> {
        let (key_addr, auth, random_key) = match addr_type {
            AddressType::DEFAULT => {
                let key_addr = Did::from_str(address.as_str())?;
                let (auth, random_key) =
                    SessionManager::gen_unsign_info(key_addr, None, Some(Signer::EIP712));
                (key_addr, auth, random_key)
            }
            AddressType::ED25519 => {
                let pubkey = PublicKey::try_from_b58t(&address).map_err(JsError::from)?;
                let (auth, random_key) =
                    SessionManager::gen_unsign_info_with_ed25519_pubkey(None, pubkey)?;
                (pubkey.address().into(), auth, random_key)
            }
        };
        Ok(UnsignedInfo {
            auth,
            random_key,
            key_addr,
        })
    }

    #[wasm_bindgen(getter)]
    pub fn auth(&self) -> Result<String, JsError> {
        let s = self.auth.to_string()?;
        Ok(s)
    }
}

/// rings-node browser client
/// the process of initialize client.
/// ``` typescript
/// const unsignedInfo = new UnsignedInfo(account);
/// const signed = await signer.signMessage(unsignedInfo.auth);
/// const sig = new Uint8Array(web3.utils.hexToBytes(signed));
/// const client: Client = await Client.new_client(unsignedInfo, sig, stunOrTurnUrl);
/// ```
#[wasm_bindgen]
#[derive(Clone)]
#[allow(dead_code)]
pub struct Client {
    processor: Arc<Processor>,
    unsigned_info: UnsignedInfo,
    signed_data: Vec<u8>,
    stuns: String,
    rpc_meta: RpcMeta,
}

#[wasm_bindgen]
impl Client {
    /// Creat a new client instance.
    pub fn new_client(
        unsigned_info: &UnsignedInfo,
        signed_data: js_sys::Uint8Array,
        stuns: String,
    ) -> js_sys::Promise {
        let unsigned_info = unsigned_info.clone();
        Self::new_client_with_storage(&unsigned_info, signed_data, stuns, "rings-node".to_owned())
    }

    /// get self web3 address
    #[wasm_bindgen(getter)]
    pub fn address(&self) -> Result<String, JsError> {
        Ok(self.processor.did().into_token().to_string())
    }

    pub fn new_client_with_storage(
        unsigned_info: &UnsignedInfo,
        signed_data: js_sys::Uint8Array,
        stuns: String,
        storage_name: String,
    ) -> js_sys::Promise {
        let unsigned_info = unsigned_info.clone();
        let signed_data = signed_data.to_vec();
        future_to_promise(async move {
            let storage = PersistenceStorage::new_with_cap_and_name(50000, storage_name.as_str())
                .await
                .map_err(JsError::from)?;

            let random_key = unsigned_info.random_key;
            let session_manager =
                SessionManager::new(&signed_data, &unsigned_info.auth, &random_key);

            let swarm = Arc::new(
                SwarmBuilder::new(&stuns, storage)
                    .session_manager(unsigned_info.key_addr, session_manager)
                    .build()
                    .map_err(JsError::from)?,
            );

            let stabilization = Arc::new(Stabilization::new(swarm.clone(), 20));
            let processor = Arc::new(Processor::from((swarm, stabilization)));
            let rpc_meta = (processor.clone(), false).into();
            Ok(JsValue::from(Client {
                processor,
                unsigned_info: unsigned_info.clone(),
                signed_data,
                stuns,
                rpc_meta,
            }))
        })
    }

    /// start backgroud listener without custom callback
    pub fn start(&self) -> js_sys::Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            let h = Arc::new(p.swarm.create_message_handler(None, None));
            let s = Arc::clone(&p.stabilization);
            futures::join!(
                async {
                    h.listen().await;
                },
                async {
                    s.wait().await;
                }
            );
            Ok(JsValue::null())
        })
    }

    /// listen message callback.
    /// ```typescript
    /// const intervalHandle = await client.listen(new MessageCallbackInstance(
    ///      async (relay: any, msg: any) => {
    ///        console.group('on custom message')
    ///        console.log(relay)
    ///        console.log(msg)
    ///        console.groupEnd()
    ///      }, async (
    ///        relay: any,
    ///      ) => {
    ///        console.group('on builtin message')
    ///        console.log(relay)
    ///        console.groupEnd()
    ///      },
    /// ))
    /// ```
    pub fn listen(&mut self, callback: MessageCallbackInstance) -> js_sys::Promise {
        let p = self.processor.clone();
        let cb = Box::new(callback);

        future_to_promise(async move {
            let h = Arc::new(p.swarm.create_message_handler(Some(cb), None));
            let s = Arc::clone(&p.stabilization);
            futures::join!(
                async {
                    h.listen().await;
                },
                async {
                    s.wait().await;
                }
            );
            Ok(JsValue::null())
        })
    }

    /// connect peer with remote jsonrpc-server url
    pub fn connect_peer_via_http(&self, remote_url: String) -> js_sys::Promise {
        log::debug!("remote_url: {}", remote_url);
        let p = self.processor.clone();
        future_to_promise(async move {
            let transport = p
                .connect_peer_via_http(remote_url.as_str())
                .await
                .map_err(JsError::from)?;
            log::debug!("connect_peer_via_http transport_id: {:?}", transport.id);
            Ok(JsValue::from_str(transport.id.to_string().as_str()))
        })
    }

    /// connect peer with web3 address, without waiting for transport channel connected
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
            let state = peer.transport.ice_connection_state().await;
            Ok(JsValue::try_from(&Peer::from((
                state,
                peer.did,
                peer.transport.id,
                peer.transport.pubkey().await,
            )))?)
        })
    }

    /// connect peer with web3 address, and wait for transport channel connected
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
            let state = peer.transport.ice_connection_state().await;
            Ok(JsValue::try_from(&Peer::from((
                state,
                peer.did,
                peer.transport.id,
                peer.transport.pubkey().await,
            )))?)
        })
    }

    /// Manually make handshake with remote peer
    pub fn create_offer(&self) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let peer = p.create_offer().await.map_err(JsError::from)?;
            Ok(JsValue::try_from(&TransportAndIce::from(peer))?)
        })
    }

    /// Manually make handshake with remote peer
    pub fn answer_offer(&self, ice_info: String) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let (transport, handshake_info) = p
                .answer_offer(ice_info.as_str())
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::try_from(&TransportAndIce::from((
                transport,
                handshake_info,
            )))?)
        })
    }

    /// Manually make handshake with remote peer
    pub fn accept_answer(&self, transport_id: String, ice: String) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let peer = p
                .accept_answer(transport_id.as_str(), ice.as_str())
                .await
                .map_err(JsError::from)?;
            let state = peer.transport.ice_connection_state().await;
            Ok(JsValue::try_from(&Peer::from((
                state,
                peer.did,
                peer.transport.id,
                peer.transport.pubkey().await,
            )))?)
        })
    }

    /// list all connect peers
    pub fn list_peers(&self) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let peers = p.list_peers().await.map_err(JsError::from)?;
            let states_async = peers
                .iter()
                .map(|x| async {
                    (
                        x.transport.ice_connection_state().await,
                        x.transport.pubkey().await,
                    )
                })
                .collect::<Vec<_>>();
            let states = futures::future::join_all(states_async).await;
            let mut js_array = js_sys::Array::new();
            js_array.extend(peers.iter().zip(states.iter()).flat_map(|(x, (y, z))| {
                JsValue::try_from(&Peer::from((*y, x.did.clone(), x.transport.id, *z)))
            }));
            Ok(js_array.into())
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

    pub fn list_pendings(&self) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let pendings = p.list_pendings().await.map_err(JsError::from)?;
            let mut js_array = js_sys::Array::new();
            js_array.extend(
                pendings
                    .into_iter()
                    .map(|x| JsValue::from_str(x.id.to_string().as_str())),
            );
            Ok(js_array.into())
        })
    }

    pub fn close_pending_transport(&self, transport_id: String) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            p.close_pending_transport(transport_id.as_str())
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from_str(&transport_id))
        })
    }

    /// send custome message to peer.
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
            let state = peer.transport.ice_connection_state().await;
            Ok(JsValue::try_from(&Peer::from((
                state,
                peer.did,
                peer.transport.id,
                peer.transport.pubkey().await,
            )))?)
        })
    }

    /// get transport state by address
    pub fn transport_state(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            let transport = p
                .swarm
                .get_transport(did)
                .ok_or_else(|| JsError::new("transport not found"))?;
            let state = transport
                .ice_connection_state()
                .await
                .map(from_rtc_ice_connection_state);
            Ok(JsValue::from_serde(&state).map_err(JsError::from)?)
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
            log::debug!("wait for data channel open start");
            if let Err(e) = peer.transport.wait_for_data_channel_open().await {
                log::warn!("wait_for_data_channel failed: {}", e);
            }
            //.map_err(JsError::from)?;
            log::debug!("wait for data channel open done");
            Ok(JsValue::null())
        })
    }

    pub fn check_cache(&self, address: String, addr_type: Option<AddressType>) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            let v_node = p.check_cache(&did).await;
            if let Some(v) = v_node {
                let wasm_vnode = VirtualNode::from(v);
                let data = JsValue::from_serde(&wasm_vnode).map_err(JsError::from)?;
                Ok(data)
            } else {
                Ok(JsValue::null())
            }
        })
    }

    pub fn fetch(&self, address: String, addr_type: Option<AddressType>) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            p.fetch(&did).await.map_err(JsError::from)?;
            Ok(JsValue::null())
        })
    }

    /// store virtual node on DHT
    pub fn store(&self, data: String) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let vnode_info = vnode::VirtualNode::try_from(data).map_err(JsError::from)?;
            p.store(vnode_info).await.map_err(JsError::from)?;
            Ok(JsValue::null())
        })
    }

    /// send http message to node.
    ///     * destination is the did of node
    ///     * method:
    ///         * `GET`
    ///         * `POST`
    ///         * `PUT`
    ///         * `DELETE`
    ///         * `HEAD`
    ///         * `OPTIONS`
    ///         * `CONNECT`
    ///         * `PATCH`
    ///         * `TRACE`
    ///     * path
    ///     * headers:
    ///     * body
    pub fn send_http(
        &self,
        destination: String,
        method: String,
        path: String,
        headers: JsValue,
        body: Option<js_sys::Uint8Array>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            let headers = if headers.is_null() {
                HashMap::new()
            } else if headers.is_object() {
                let mut header_map: HashMap<String, String> = HashMap::new();
                let obj = js_sys::Object::from(headers);
                let entries = js_sys::Object::entries(&obj);
                for e in entries.iter() {
                    if js_sys::Array::is_array(&e) {
                        let arr = js_sys::Array::from(&e);
                        if arr.length() != 2 {
                            continue;
                        }
                        let k = arr.get(0);
                        let v = arr.get(1);
                        if v.is_string() {
                            header_map.insert(k.as_string().unwrap(), v.as_string().unwrap());
                        }
                    }
                }
                header_map
            } else {
                HashMap::new()
            };

            let body = body.map(|b| bytes::Bytes::from(b.to_vec()));

            let msg = BackendMessage::HttpServer(HttpServerMessage::Request(HttpServerRequest {
                method,
                path: path.to_owned(),
                headers,
                body,
            }));
            let text = serde_json::to_string(&msg).unwrap();

            let tx_id = p
                .send_message(destination.as_str(), text.as_bytes())
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from_str(tx_id.to_string().as_str()))
        })
    }
}

#[wasm_bindgen]
pub struct MessageCallbackInstance {
    custom_message: Arc<js_sys::Function>,
    http_response_message: Arc<js_sys::Function>,
    builtin_message: Arc<js_sys::Function>,
    chunlist: Arc<Mutex<chunk::ChunkList<15000>>>,
}

#[wasm_bindgen]
impl MessageCallbackInstance {
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
            chunlist: Arc::new(Mutex::new(chunk::ChunkList::default())),
        })
    }
}

#[async_trait(?Send)]
impl MessageCallback for MessageCallbackInstance {
    async fn custom_message(
        &self,
        handler: &MessageHandler,
        relay: &MessagePayload<Message>,
        msg: &MaybeEncrypted<CustomMessage>,
    ) {
        let r = handler.decrypt_msg(msg);
        let msg = if let Err(e) = r {
            log::error!("custom_message decrypt failed: {:?}", e);
            return;
        } else {
            r.unwrap()
        };
        let msg_body = msg.0;
        let (left, right) = msg_body.split_at(4);

        let this = JsValue::null();

        if left[0] == 1 {
            let right_vec = right.to_vec();
            let data_opt = {
                let mut c = self.chunlist.try_lock().unwrap();
                c.as_vec_mut()
                    .extend_from_slice(chunk::ChunkList::from(&right_vec).as_vec());
                let d = c.get(relay.tx_id);
                if d.is_some() {
                    c.remove(relay.tx_id);
                }
                d
            };
            log::debug!("chunk message of {:?} received", relay.tx_id);
            if let Some(data) = data_opt {
                let msg_context = data.as_slice();
                log::info!(
                    "chunk message of {:?} received, {:?}",
                    relay.tx_id,
                    &msg_context
                );
                let msg_content = js_sys::Uint8Array::from(msg_context);
                if let Ok(r) = self.http_response_message.call2(
                    &this,
                    &JsValue::from_serde(&relay).unwrap(),
                    &msg_content,
                ) {
                    if let Ok(p) = js_sys::Promise::try_from(r) {
                        if let Err(e) = wasm_bindgen_futures::JsFuture::from(p).await {
                            log::warn!("invoke on_custom_message error: {:?}", e);
                        }
                    }
                }
            } else {
                log::info!("chunk message of {:?} not complete", relay.tx_id);
            }
            return;
        }
        log::debug!("custom_message received: {:?}", right);
        let msg_content = js_sys::Uint8Array::from(right.to_vec().as_slice());

        if let Ok(r) =
            self.custom_message
                .call2(&this, &JsValue::from_serde(&relay).unwrap(), &msg_content)
        {
            if let Ok(p) = js_sys::Promise::try_from(r) {
                if let Err(e) = wasm_bindgen_futures::JsFuture::from(p).await {
                    log::warn!("invoke on_custom_message error: {:?}", e);
                }
            }
        }
        //let a = wasm_bindgen_futures::JsFuture::from(self.on_cutom_message.as_ref().clone()).await;
    }

    async fn builtin_message(&self, _handler: &MessageHandler, relay: &MessagePayload<Message>) {
        let this = JsValue::null();
        // log::debug!("builtin_message received: {:?}", relay);
        if let Ok(r) = self
            .builtin_message
            .call1(&this, &JsValue::from_serde(&relay).unwrap())
        {
            if let Ok(p) = js_sys::Promise::try_from(r) {
                if let Err(e) = wasm_bindgen_futures::JsFuture::from(p).await {
                    log::warn!("invoke on_builtin_message error: {:?}", e);
                }
            }
        }
    }
}

#[wasm_bindgen]
#[derive(Clone, Serialize, Deserialize)]
pub struct Peer {
    address: String,
    transport_pubkey: String,
    transport_id: String,
    state: Option<String>,
}

#[wasm_bindgen]
impl Peer {
    #[wasm_bindgen(getter)]
    pub fn address(&self) -> String {
        self.address.to_owned()
    }

    #[wasm_bindgen(getter)]
    pub fn transport_id(&self) -> String {
        self.transport_id.to_owned()
    }

    #[wasm_bindgen(getter)]
    pub fn state(&self) -> Option<String> {
        self.state.to_owned()
    }

    #[wasm_bindgen(getter)]
    pub fn transport_pubkey(&self) -> String {
        self.transport_pubkey.to_owned()
    }
}

impl From<(Option<RtcIceConnectionState>, Token, Uuid, PublicKey)> for Peer {
    fn from(
        (st, address, transport_id, transport_pubkey): (
            Option<RtcIceConnectionState>,
            Token,
            Uuid,
            PublicKey,
        ),
    ) -> Self {
        Self {
            address: address.to_string(),
            transport_pubkey: Result::unwrap_or(transport_pubkey.to_base58_string(), "".to_owned()),
            transport_id: transport_id.to_string(),
            state: st.map(from_rtc_ice_connection_state),
        }
    }
}

impl TryFrom<&Peer> for JsValue {
    type Error = JsError;

    fn try_from(value: &Peer) -> Result<Self, Self::Error> {
        JsValue::from_serde(value).map_err(JsError::from)
    }
}

#[wasm_bindgen]
#[derive(Clone, Serialize, Deserialize)]
pub struct TransportAndIce {
    transport_id: String,
    ice: String,
}

#[wasm_bindgen]
impl TransportAndIce {
    pub fn new(transport_id: &str, ice: &str) -> Self {
        Self {
            transport_id: transport_id.to_owned(),
            ice: ice.to_owned(),
        }
    }

    #[wasm_bindgen(getter)]
    pub fn transport_id(&self) -> String {
        self.transport_id.to_owned()
    }

    #[wasm_bindgen(getter)]
    pub fn ice(&self) -> String {
        self.ice.to_owned()
    }
}

impl From<(Arc<Transport>, Encoded)> for TransportAndIce {
    fn from((transport, ice): (Arc<Transport>, Encoded)) -> Self {
        Self::new(transport.id.to_string().as_str(), ice.to_string().as_str())
    }
}

impl TryFrom<&TransportAndIce> for JsValue {
    type Error = JsError;

    fn try_from(value: &TransportAndIce) -> Result<Self, Self::Error> {
        JsValue::from_serde(value).map_err(JsError::from)
    }
}

#[wasm_bindgen]
#[derive(Debug, Clone)]
#[allow(dead_code)]
/// Internal Info struct
pub struct InternalInfo {
    build_version: String,
}

#[wasm_bindgen]
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
#[wasm_bindgen]
pub fn internal_info() -> InternalInfo {
    InternalInfo::build()
}

pub fn get_did(address: &str, addr_type: AddressType) -> Result<Did, JsError> {
    let did = match addr_type {
        AddressType::DEFAULT => {
            Did::from_str(address).map_err(|_| JsError::new("invalid address"))?
        }
        AddressType::ED25519 => PublicKey::try_from_b58t(address)
            .map_err(|_| JsError::new("invalid address"))?
            .address()
            .into(),
    };
    Ok(did)
}

#[wasm_bindgen]
#[derive(Clone, Serialize, Deserialize)]
pub enum VNodeType {
    /// Data: Encoded data stored in DHT
    Data,
    /// SubRing: Finger table of a SubRing
    SubRing,
    /// RelayMessage: A Relayed but unreach message, which is stored on it's successor
    RelayMessage,
}

impl From<vnode::VNodeType> for VNodeType {
    fn from(v: vnode::VNodeType) -> Self {
        match v {
            vnode::VNodeType::Data => Self::Data,
            vnode::VNodeType::SubRing => Self::SubRing,
            vnode::VNodeType::RelayMessage => Self::RelayMessage,
        }
    }
}

#[wasm_bindgen]
#[derive(Clone, Serialize, Deserialize)]
pub struct VirtualNode {
    address: String,
    data: Vec<String>,
    kind: VNodeType,
}

#[wasm_bindgen]
impl VirtualNode {
    #[wasm_bindgen(getter)]
    pub fn address(&self) -> String {
        self.address.to_owned()
    }

    #[wasm_bindgen(getter)]
    pub fn kind(&self) -> VNodeType {
        self.kind.to_owned()
    }

    #[wasm_bindgen(getter)]
    pub fn data(&self) -> js_sys::Array {
        let array_data = js_sys::Array::new();
        for d in self.data.iter() {
            array_data.push(&JsValue::from_str(d));
        }
        array_data
    }
}

impl From<vnode::VirtualNode> for VirtualNode {
    fn from(v: vnode::VirtualNode) -> Self {
        Self {
            address: v.address.into_token().to_string(),
            kind: VNodeType::Data,
            data: v.data.iter().map(|x| x.to_string()).collect(),
        }
    }
}
