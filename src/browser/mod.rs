//! rings-node browser support.
#![allow(clippy::unused_unit)]
pub mod utils;

use std::str::FromStr;
use std::sync::Arc;

use futures::lock::Mutex;
use js_sys::Promise;
use rings_core_wasm::dht::TStabilize;
use serde::Deserialize;
use serde::Serialize;

use self::utils::from_rtc_ice_connection_state;
use crate::prelude::js_sys;
use crate::prelude::rings_core::async_trait;
use crate::prelude::rings_core::dht::PeerRing;
use crate::prelude::rings_core::dht::Stabilization;
use crate::prelude::rings_core::ecc::SecretKey;
use crate::prelude::rings_core::message::CustomMessage;
use crate::prelude::rings_core::message::Encoded;
use crate::prelude::rings_core::message::MaybeEncrypted;
use crate::prelude::rings_core::message::Message;
use crate::prelude::rings_core::message::MessageCallback;
use crate::prelude::rings_core::message::MessageHandler;
use crate::prelude::rings_core::message::MessagePayload;
use crate::prelude::rings_core::prelude::web3::types::Address;
use crate::prelude::rings_core::session::AuthorizedInfo;
use crate::prelude::rings_core::session::SessionManager;
use crate::prelude::rings_core::session::Signer;
use crate::prelude::rings_core::storage::PersistenceStorage;
use crate::prelude::rings_core::swarm::Swarm;
use crate::prelude::rings_core::swarm::TransportManager;
use crate::prelude::rings_core::transports::Transport;
use crate::prelude::rings_core::types::ice_transport::IceTransport;
use crate::prelude::rings_core::types::message::MessageListener;
use crate::prelude::wasm_bindgen;
use crate::prelude::wasm_bindgen::prelude::*;
use crate::prelude::wasm_bindgen_futures;
use crate::prelude::wasm_bindgen_futures::future_to_promise;
use crate::prelude::web3::contract::tokens::Tokenizable;
use crate::prelude::web_sys::RtcIceConnectionState;
use crate::processor;
use crate::processor::Processor;

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsError> {
    utils::set_panic_hook();
    Ok(())
}

/// set debug for wasm.
/// if `true` will print `Debug` message in console,
/// otherwise only print `error` message
#[wasm_bindgen]
pub fn debug(value: bool) {
    if value {
        console_log::init_with_level(log::Level::Debug).ok();
    } else {
        console_log::init_with_level(log::Level::Error).ok();
    }
}

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
#[derive(Clone)]
pub struct UnsignedInfo {
    key_addr: Address,
    auth: AuthorizedInfo,
    random_key: SecretKey,
}

#[wasm_bindgen]
impl UnsignedInfo {
    /// Create a new `UnsignedInfo` instance with SignerMode::EIP712
    #[wasm_bindgen(constructor)]
    pub fn new(key_addr: String) -> Result<UnsignedInfo, JsError> {
        Self::new_with_signer(key_addr, Some(SignerMode::EIP712))
    }

    /// Create a new `UnsignedInfo` instance
    ///   * key_addr: wallet address
    ///   * signer: `SignerMode`
    pub fn new_with_signer(
        key_addr: String,
        signer: Option<SignerMode>,
    ) -> Result<UnsignedInfo, JsError> {
        let key_addr = Address::from_str(key_addr.as_str())?;
        let (auth, random_key) = SessionManager::gen_unsign_info(
            key_addr,
            None,
            Some(signer.unwrap_or(SignerMode::EIP712).into()),
        )?;
        Ok(UnsignedInfo {
            key_addr,
            auth,
            random_key,
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
/// const client = new Client(unsignedInfo, sig, stunOrTurnUrl);
/// ```
#[wasm_bindgen]
#[derive(Clone)]
#[allow(dead_code)]
pub struct Client {
    processor: Arc<Processor>,
    unsigned_info: UnsignedInfo,
    signed_data: Vec<u8>,
    stuns: String,
}

#[wasm_bindgen]
impl Client {
    // #[wasm_bindgen(constructor)]
    // pub fn new(
    //     unsigned_info: &UnsignedInfo,
    //     signed_data: js_sys::Uint8Array,
    //     stuns: String,
    // ) -> Result<Client, JsError> {
    //     // Self {
    //     //     processor: None,
    //     //     unsigned_info: unsigned_info.clone(),
    //     //     signed_data: signed_data.to_vec(),
    //     //     stuns,
    //     // }
    //     let random_key = unsigned_info.random_key;
    //     let session = SessionManager::new(&signed_data.to_vec(), &unsigned_info.auth, &random_key);
    //     let swarm = Arc::new(Swarm::new(&stuns, unsigned_info.key_addr, session));

    //     let mut pr = Option::None;

    //     wasm_bindgen_futures::spawn_local(async{
    //         pr = Some(PeerRing::new_with_storage(
    //             swarm.address().into(),
    //             //storage.inner.clone().unwrap().clone(),
    //             Arc::new(PersistenceStorage::new().await.unwrap()),
    //         ));
    //     });
    //     let pr = pr.unwrap();

    //     let dht = Arc::new(Mutex::new(pr));
    //     let msg_handler = Arc::new(MessageHandler::new(dht.clone(), swarm.clone()));
    //     let stabilization = Arc::new(Stabilization::new(dht, swarm.clone(), 20));
    //     let processor = Arc::new(Processor::from((swarm, msg_handler, stabilization)));
    //     Ok(Client {
    //         processor,
    //         unsigned_info: unsigned_info.clone(),
    //         signed_data: signed_data.to_vec(),
    //         stuns,
    //     })
    // }

    pub fn new_client(
        unsigned_info: &UnsignedInfo,
        signed_data: js_sys::Uint8Array,
        stuns: String,
    ) -> Promise {
        let unsigned_info = unsigned_info.clone();
        Self::new_client_with_storage(&unsigned_info, signed_data, stuns, "rings-node".to_owned())
    }

    pub fn new_client_with_storage(
        unsigned_info: &UnsignedInfo,
        signed_data: js_sys::Uint8Array,
        stuns: String,
        storage_name: String,
    ) -> Promise {
        let unsigned_info = unsigned_info.clone();
        let signed_data = signed_data.to_vec();
        future_to_promise(async move {
            let random_key = unsigned_info.random_key;
            let session = SessionManager::new(&signed_data, &unsigned_info.auth, &random_key);
            let swarm = Arc::new(Swarm::new(&stuns, unsigned_info.key_addr, session));

            let storage = PersistenceStorage::new_with_cap_and_name(50000, storage_name.as_str())
                .await
                .map_err(JsError::from)?;
            let pr = PeerRing::new_with_storage(swarm.address().into(), Arc::new(storage));

            let dht = Arc::new(Mutex::new(pr));
            let msg_handler = Arc::new(MessageHandler::new(dht.clone(), swarm.clone()));
            let stabilization = Arc::new(Stabilization::new(dht, swarm.clone(), 20));
            let processor = Arc::new(Processor::from((swarm, msg_handler, stabilization)));
            Ok(JsValue::from(Client {
                processor,
                unsigned_info: unsigned_info.clone(),
                signed_data,
                stuns,
            }))
        })
    }

    // pub fn build(&mut self) -> Promise {
    //     let p = self.processor.clone();
    //     future_to_promise(async {
    //         let random_key = self.unsigned_info.random_key;
    //         let session =
    //             SessionManager::new(&self.signed_data, &self.unsigned_info.auth, &random_key);
    //         let swarm = Arc::new(Swarm::new(
    //             &self.stuns,
    //             self.unsigned_info.key_addr,
    //             session,
    //         ));

    //         let pr = PeerRing::new(swarm.address().into())
    //             .await
    //             .map_err(JsError::from)?;

    //         let dht = Arc::new(Mutex::new(pr));
    //         let msg_handler = Arc::new(MessageHandler::new(dht.clone(), swarm.clone()));
    //         let stabilization = Arc::new(Stabilization::new(dht, swarm.clone(), 20));
    //         let processor = Arc::new(Processor::from((swarm, msg_handler, stabilization)));
    //         //self.processor = Some(processor);
    //         Ok(JsValue::null())
    //     })
    // }

    /// start backgroud listener without custom callback
    pub fn start(&self) -> Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            let h = Arc::clone(&p.msg_handler);
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

    /// get self web3 address
    #[wasm_bindgen(getter)]
    pub fn address(&self) -> Result<String, JsError> {
        Ok(self.processor.address().into_token().to_string())
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
    pub fn listen(&mut self, callback: MessageCallbackInstance) -> Promise {
        let p = self.processor.clone();
        let cb = Box::new(callback);

        future_to_promise(async move {
            let h = Arc::clone(&p.msg_handler);
            let s = Arc::clone(&p.stabilization);
            h.set_callback(cb).await;
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
    pub fn connect_peer_via_http(&self, remote_url: String) -> Promise {
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
    pub fn connect_with_address_without_wait(&self, address: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let address =
                Address::from_str(address.as_str()).map_err(|_| JsError::new("invalid address"))?;
            let peer = p
                .connect_with_address(&address, false)
                .await
                .map_err(JsError::from)?;
            let state = peer.transport.ice_connection_state().await;
            Ok(JsValue::try_from(&Peer::from((state, peer)))?)
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
    /// await client1.connect_with_address(client3.address())
    /// ```
    pub fn connect_with_address(&self, address: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let address =
                Address::from_str(address.as_str()).map_err(|_| JsError::new("invalid address"))?;
            let peer = p
                .connect_with_address(&address, true)
                .await
                .map_err(JsError::from)?;
            let state = peer.transport.ice_connection_state().await;
            Ok(JsValue::try_from(&Peer::from((state, peer)))?)
        })
    }

    /// Manually make handshake with remote peer
    pub fn create_offer(&self) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let peer = p.create_offer().await.map_err(JsError::from)?;
            Ok(JsValue::try_from(&TransportAndIce::from(peer))?)
        })
    }

    /// Manually make handshake with remote peer
    pub fn answer_offer(&self, ice_info: String) -> Promise {
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
    pub fn accept_answer(&self, transport_id: String, ice: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let peer = p
                .accept_answer(transport_id.as_str(), ice.as_str())
                .await
                .map_err(JsError::from)?;
            let state = peer.transport.ice_connection_state().await;
            Ok(JsValue::try_from(&Peer::from((state, peer)))?)
        })
    }

    /// list all connect peers
    pub fn list_peers(&self) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let peers = p.list_peers().await.map_err(JsError::from)?;
            let states_async = peers
                .iter()
                .map(|x| x.transport.ice_connection_state())
                .collect::<Vec<_>>();
            let states = futures::future::join_all(states_async).await;
            let mut js_array = js_sys::Array::new();
            js_array.extend(
                peers
                    .iter()
                    .zip(states.iter())
                    .flat_map(|(x, y)| JsValue::try_from(&Peer::from((*y, x.clone())))),
            );
            Ok(js_array.into())
        })
    }

    /// disconnect a peer with web3 address
    pub fn disconnect(&self, address: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            p.disconnect(address.as_str())
                .await
                .map_err(JsError::from)?;

            Ok(JsValue::from_str(address.to_string().as_str()))
        })
    }

    pub fn list_pendings(&self) -> Promise {
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

    pub fn close_pending_transport(&self, transport_id: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            p.close_pending_transport(transport_id.as_str())
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from_str(&transport_id))
        })
    }

    /// send custome message to peer.
    pub fn send_message(&self, destination: String, msg: js_sys::Uint8Array) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            p.send_message(destination.as_str(), &msg.to_vec())
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from_bool(true))
        })
    }

    /// get peer by address
    pub fn get_peer(&self, address: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let peer = p.get_peer(address.as_str()).await.map_err(JsError::from)?;
            let state = peer.transport.ice_connection_state().await;
            Ok(JsValue::try_from(&Peer::from((state, peer)))?)
        })
    }

    /// get transport state by address
    pub fn transport_state(&self, address: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let address = Address::from_str(address.as_str()).map_err(JsError::from)?;
            let transport = p
                .swarm
                .get_transport(&address)
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
    pub fn wait_for_data_channel_open(&self, address: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            log::debug!("address: {}", address);
            let peer = p.get_peer(address.as_str()).await.map_err(JsError::from)?;
            log::debug!("wait for data channel open start");
            if let Err(e) = peer.transport.wait_for_data_channel_open().await {
                log::warn!("wait_for_data_channel failed: {}", e);
            }
            //.map_err(JsError::from)?;
            log::debug!("wait for data channel open done");
            Ok(JsValue::null())
        })
    }
}

#[wasm_bindgen]
pub struct MessageCallbackInstance {
    custom_message: Arc<js_sys::Function>,
    builtin_message: Arc<js_sys::Function>,
}

#[wasm_bindgen]
impl MessageCallbackInstance {
    #[wasm_bindgen(constructor)]
    pub fn new(
        custom_message: &js_sys::Function,
        builtin_message: &js_sys::Function,
    ) -> Result<MessageCallbackInstance, JsError> {
        Ok(MessageCallbackInstance {
            custom_message: Arc::new(custom_message.clone()),
            builtin_message: Arc::new(builtin_message.clone()),
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
        log::debug!("custom_message received: {:?}", msg);

        let r = handler.decrypt_msg(msg);
        let msg = if let Err(e) = r {
            log::error!("custom_message decrypt failed: {:?}", e);
            return;
        } else {
            r.unwrap()
        };
        // let msg = r.unwrap();

        let this = JsValue::null();
        let msg = js_sys::Uint8Array::from(&msg.0[..]);

        if let Ok(r) = self
            .custom_message
            .call2(&this, &JsValue::from_serde(&relay).unwrap(), &msg)
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
        log::debug!("builtin_message received: {:?}", relay);
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
}

impl From<(Option<RtcIceConnectionState>, processor::Peer)> for Peer {
    fn from((st, p): (Option<RtcIceConnectionState>, processor::Peer)) -> Self {
        Self {
            address: p.address.to_string(),
            transport_id: p.transport.id.to_string(),
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
