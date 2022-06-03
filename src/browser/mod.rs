//! rings-node browser support.
#![allow(clippy::unused_unit)]
pub mod utils;

use crate::{
    prelude::rings_core::{
        async_trait,
        dht::{Did, PeerRing},
        ecc::SecretKey,
        message::{
            CustomMessage, Encoded, MaybeEncrypted, Message, MessageCallback, MessageHandler,
            MessageRelay,
        },
        prelude::web3::types::Address,
        session::SessionManager,
        session::{AuthorizedInfo, Signer},
        swarm::{Swarm, TransportManager},
        transports::Transport,
        types::{ice_transport::IceTransport, message::MessageListener},
    },
    processor::{self, Processor},
};
use futures::lock::Mutex;
use js_sys::Promise;
use serde::{Deserialize, Serialize};
use std::{str::FromStr, sync::Arc};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{future_to_promise, spawn_local};

#[wasm_bindgen]
extern "C" {

    fn setInterval(closure: &Closure<dyn FnMut()>, time: u32) -> i32;

    fn clearInterval(id: i32);
}

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
#[derive(Clone)]
pub struct UnsignedInfo {
    key_addr: Address,
    auth: AuthorizedInfo,
    random_key: SecretKey,
}

#[wasm_bindgen]
impl UnsignedInfo {
    #[wasm_bindgen(constructor)]
    pub fn new(key_addr: String) -> Result<UnsignedInfo, JsError> {
        let key_addr = Address::from_str(key_addr.as_str())?;
        let (auth, random_key) =
            SessionManager::gen_unsign_info(key_addr, None, Some(Signer::EIP712))?;
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
pub struct Client {
    processor: Arc<Processor>,
    message_handler: Option<Arc<MessageHandler>>,
}

#[wasm_bindgen]
impl Client {
    #[wasm_bindgen(constructor)]
    pub fn new(
        unsigned_info: &UnsignedInfo,
        signed_data: js_sys::Uint8Array,
        stuns: String,
    ) -> Result<Client, JsError> {
        let random_key = unsigned_info.random_key;
        let session = SessionManager::new(&signed_data.to_vec(), &unsigned_info.auth, &random_key);
        let swarm = Arc::new(Swarm::new(&stuns, unsigned_info.key_addr, session));
        let pr = PeerRing::new(swarm.address().into());
        let dht = Arc::new(Mutex::new(pr));
        let msg_handler = Arc::new(MessageHandler::new(dht, swarm.clone()));
        let processor = Arc::new(Processor::from((swarm, msg_handler)));
        Ok(Client {
            processor,
            message_handler: None,
        })
    }

    pub fn start(&self) -> Promise {
        let msg_handler = self.processor.msg_handler.clone();
        future_to_promise(async move {
            msg_handler.listen().await;
            Ok(JsValue::from_str(""))
        })
    }

    /// listen message callback.
    /// ```typescript
    /// await client.listen(new MessageCallbackInstance(
    ///      async (relay: any, prev: String, msg: any) => {
    ///        console.group('on custom message')
    ///        console.log(relay)
    ///        console.log(prev)
    ///        console.log(msg)
    ///        console.groupEnd()
    ///      }, async (
    ///        relay: any, prev: String,
    ///      ) => {
    ///        console.group('on builtin message')
    ///        console.log(relay)
    ///        console.log(prev)
    ///        console.groupEnd()
    ///      },
    /// ))
    /// ```
    pub fn listen(&mut self, callback: MessageCallbackInstance) -> Result<IntervalHandle, JsError> {
        let p = self.processor.clone();
        let pr = PeerRing::new(p.swarm.address().into());
        log::debug!("peer_ring: {:?}", pr.id);
        let dht = Arc::new(Mutex::new(pr));
        let msg_handler = Arc::new(MessageHandler::new_with_callback(
            dht,
            p.swarm.clone(),
            Box::new(callback),
        ));
        self.message_handler = Some(msg_handler.clone());
        let h = Arc::clone(&msg_handler);

        let cb = Closure::wrap(Box::new(move || {
            let h1 = h.clone();
            spawn_local(async move {
                h1.clone().listen_once().await;
                // console_log!("listen_once: {:?}", a);
            });
        }) as Box<dyn FnMut()>);

        // Next we pass this via reference to the `setInterval` function, and
        // `setInterval` gets a handle to the corresponding JS closure.
        let interval_id = setInterval(&cb, 200);

        // If we were to drop `cb` here it would cause an exception to be raised
        // whenever the interval elapses. Instead we *return* our handle back to JS
        // so JS can decide when to cancel the interval and deallocate the closure.
        Ok(IntervalHandle {
            interval_id,
            _closure: cb,
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

    /// connect peer with web3 address
    pub fn connect_with_address(&self, address: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let address =
                Address::from_str(address.as_str()).map_err(|_| JsError::new("invalid address"))?;
            p.connect_with_address(&address)
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::null())
        })
    }

    /// Manually make handshke with remote peer
    pub fn create_offer(&self) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let peer = p.create_offer().await.map_err(JsError::from)?;
            Ok(TransportAndIce::from(peer).into())
        })
    }

    /// Manually make handshke with remote peer
    pub fn answer_offer(&self, ice_info: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let (transport, handshake_info) = p
                .answer_offer(ice_info.as_str())
                .await
                .map_err(JsError::from)?;
            Ok(TransportAndIce::from((transport, handshake_info)).into())
        })
    }

    /// Manually make handshke with remote peer
    pub fn accept_answer(&self, transport_id: String, ice: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let peer = p
                .accept_answer(transport_id.as_str(), ice.as_str())
                .await
                .map_err(JsError::from)?;
            Ok(Peer::from(peer).into())
        })
    }

    /// list all connect peers
    pub fn list_peers(&self) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let peers = p.list_peers().await.map_err(JsError::from)?;
            let mut js_array = js_sys::Array::new();
            js_array.extend(peers.into_iter().map(|x| JsValue::from(Peer::from(x))));
            Ok(js_array.into())
        })
    }

    /// disconnect a peer with web3 address
    pub fn disconnect(&self, address: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let address = Address::from_str(address.as_str()).map_err(JsError::from)?;
            let trans = p
                .swarm
                .get_transport(&address)
                .ok_or_else(|| JsError::new("transport not found"))?;
            trans.close().await.map_err(JsError::from)?;
            p.swarm.remove_transport(&address);
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
            Ok(transport_id.into())
        })
    }

    /// send custome message to peer.
    pub fn send_message(&self, next_hop: String, destination: String, msg: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            p.send_message(next_hop.as_str(), destination.as_str(), msg)
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from_bool(true))
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
        relay: &MessageRelay<Message>,
        prev: Did,
        msg: &MaybeEncrypted<CustomMessage>,
    ) {
        log::debug!("custom_message received: {:?}", msg);

        let r = handler.decrypt_msg(msg);
        if let Err(e) = r {
            log::error!("custom_message decrypt failed: {:?}", e);
            return;
        }
        let msg = r.unwrap();

        let this = JsValue::null();

        if let Ok(r) = self.custom_message.call3(
            &this,
            &JsValue::from_serde(&relay).unwrap(),
            &JsValue::from_str(prev.to_string().as_str()),
            &JsValue::from_serde(&msg).unwrap(),
        ) {
            if let Ok(p) = js_sys::Promise::try_from(r) {
                if let Err(e) = wasm_bindgen_futures::JsFuture::from(p).await {
                    log::warn!("invoke on_custom_message error: {:?}", e);
                }
            }
        }
        //let a = wasm_bindgen_futures::JsFuture::from(self.on_cutom_message.as_ref().clone()).await;
    }

    async fn builtin_message(
        &self,
        _handler: &MessageHandler,
        relay: &MessageRelay<Message>,
        prev: Did,
    ) {
        let this = JsValue::null();
        log::debug!("builtin_message received: {:?}", relay);
        if let Ok(r) = self.builtin_message.call2(
            &this,
            &JsValue::from_serde(&relay).unwrap(),
            &JsValue::from_str(prev.to_string().as_str()),
        ) {
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
}

impl From<processor::Peer> for Peer {
    fn from(p: processor::Peer) -> Self {
        Self {
            address: p.address.to_string(),
            transport_id: p.transport.id.to_string(),
        }
    }
}

#[wasm_bindgen]
#[derive(Clone, Serialize, Deserialize)]
#[allow(dead_code)]
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

#[wasm_bindgen]
pub struct IntervalHandle {
    interval_id: i32,
    _closure: Closure<dyn FnMut()>,
}

impl Drop for IntervalHandle {
    fn drop(&mut self) {
        clearInterval(self.interval_id);
    }
}
