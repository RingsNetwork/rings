#![allow(clippy::unused_unit)]
pub mod utils;

use crate::{
    prelude::rings_core::{
        dht::PeerRing,
        ecc::SecretKey,
        message::{Encoded, MessageHandler},
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
use wasm_bindgen_futures::future_to_promise;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn log(a: &str);
}

macro_rules! console_log {
    ($($t:tt)*) => (log(&format_args!($($t)*).to_string()))
}

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsError> {
    utils::set_panic_hook();
    Ok(())
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

#[wasm_bindgen]
#[derive(Clone)]
pub struct Client {
    processor: Arc<Processor>,
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
        let processor = Arc::new(Processor::from(swarm));
        Ok(Client { processor })
    }

    pub fn start(&self) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let pr = PeerRing::new(p.swarm.address().into());
            console_log!("peer_ring: {:?}", pr.id);
            let dht = Arc::new(Mutex::new(pr.clone()));
            let msg_handler = Arc::new(MessageHandler::new(dht.clone(), p.swarm.clone()));
            msg_handler.listen().await;
            Ok(JsValue::from_str(pr.id.to_string().as_str()))
        })
    }

    pub fn connect_peer_via_http(&self, remote_url: String) -> Promise {
        console_log!("remote_url: {}", remote_url);
        let p = self.processor.clone();
        future_to_promise(async move {
            let transport = p
                .connect_peer_via_http(remote_url.as_str())
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from_str(transport.id.to_string().as_str()))
        })
    }

    pub fn connect_peer_via_ice(&self, ice_info: String) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let (transport, handshake_info) = p
                .connect_peer_via_ice(ice_info.as_str())
                .await
                .map_err(JsError::from)?;
            Ok(TransportAndIce::from((transport, handshake_info)).into())
        })
    }

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

    pub fn list_peers(&self) -> Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let peers = p.list_peers().await.map_err(JsError::from)?;
            let mut js_array = js_sys::Array::new();
            js_array.extend(peers.into_iter().map(|x| JsValue::from(Peer::from(x))));
            Ok(js_array.into())
        })
    }

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
