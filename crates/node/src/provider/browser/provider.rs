#![warn(missing_docs)]
//! Browser Provider implementation
#![allow(non_snake_case, non_upper_case_globals, clippy::ptr_offset_with_cast)]
use std::convert::TryFrom;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use js_sys;
use js_sys::Uint8Array;
use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::prelude::vnode;
use rings_core::prelude::vnode::VirtualNode;
use rings_core::storage::idb::IdbStorage;
use rings_core::utils::js_utils;
use rings_core::utils::js_value;
use rings_derive::wasm_export;
use rings_rpc::protos::rings_node::*;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::WebrtcConnectionState;
use serde::Deserialize;
use serde::Serialize;
use wasm_bindgen;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures;
use wasm_bindgen_futures::future_to_promise;
use wasm_bindgen_futures::JsFuture;

use crate::backend::browser::BackendBehaviour;
use crate::backend::snark::SNARKBehaviour;
use crate::backend::types::BackendMessage;
use crate::backend::types::HttpRequest;
use crate::backend::types::ServiceMessage;
use crate::backend::Backend;
use crate::processor::ProcessorConfig;
use crate::provider::AsyncSigner;
use crate::provider::Provider;
use crate::provider::Signer;

/// AddressType enum contains `DEFAULT` and `ED25519`.
#[wasm_export]
pub enum AddressType {
    /// Default address type, hex string of sha1(pubkey)
    DEFAULT,
    /// Ed25519 style address type, hex string of pubkey
    Ed25519,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Peer {
    pub did: String,
    pub state: String,
}

impl Peer {
    fn new(did: Did, state: WebrtcConnectionState) -> Self {
        Self {
            did: did.to_string(),
            state: format!("{:?}", state),
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
impl Provider {
    /// Create new instance of Provider, return Promise
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
        stabilize_timeout: u64,
        account: String,
        account_type: String,
        signer: js_sys::Function,
        backend_behaviour: Option<BackendBehaviour>,
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

            let vnode_storage = Box::new(
                IdbStorage::new_with_cap_and_name(50000, "rings-node")
                    .await
                    .expect("Failed on create vnode storage"),
            );

            let measure_storage = Box::new(
                IdbStorage::new_with_cap_and_name(50000, "rings-node/measure")
                    .await
                    .expect("Failed on create measure storage"),
            );

            let provider = Provider::new_provider_internal(
                ice_servers,
                stabilize_timeout,
                account,
                account_type,
                Signer::Async(Box::new(signer)),
                Some(vnode_storage),
                Some(measure_storage),
            )
            .await?;

            if let Some(cb) = backend_behaviour {
                let backend: Backend = Backend::new(Arc::new(provider.clone()), Box::new(cb));
                provider
                    .set_swarm_callback_internal(Arc::new(backend))
                    .expect("Failed on set swarm callback");
            }

            Ok(JsValue::from(provider))
        })
    }

    /// Create new provider instance with serialized config (yaml/json)
    pub fn new_provider_with_serialized_config(
        config: String,
        backend: Option<BackendBehaviour>,
    ) -> js_sys::Promise {
        let cfg: ProcessorConfig = serde_yaml::from_str(&config).unwrap();
        Self::new_provider_with_config(cfg, backend)
    }

    /// Create a new provider instance.
    pub fn new_provider_with_config(
        config: ProcessorConfig,
        backend: Option<BackendBehaviour>,
    ) -> js_sys::Promise {
        Self::new_provider_with_storage(config, backend, "rings-node".to_string())
    }

    /// get self web3 address
    #[wasm_bindgen(getter)]
    pub fn address(&self) -> String {
        self.processor.did().to_string()
    }

    ///  create new unsigned Provider
    pub fn new_provider_with_storage(
        config: ProcessorConfig,
        backend_behaviour: Option<BackendBehaviour>,
        storage_name: String,
    ) -> js_sys::Promise {
        future_to_promise(async move {
            let vnode_storage = Box::new(
                IdbStorage::new_with_cap_and_name(50000, &storage_name)
                    .await
                    .expect("Failed on create vnode storage"),
            );

            let measure_storage = Box::new(
                IdbStorage::new_with_cap_and_name(50000, &format!("{}/measure", storage_name))
                    .await
                    .expect("Failed on create measure storage"),
            );

            let provider = Self::new_provider_with_storage_internal(
                config,
                Some(vnode_storage),
                Some(measure_storage),
            )
            .await
            .map_err(JsError::from)?;
            if let Some(cb) = backend_behaviour {
                let backend: Backend = Backend::new(Arc::new(provider.clone()), Box::new(cb));
                provider
                    .set_swarm_callback_internal(Arc::new(backend))
                    .expect("Failed on set swarm callback");
            }
            Ok(JsValue::from(provider))
        })
    }

    /// Request local rpc interface
    pub fn request(&self, method: String, params: JsValue) -> js_sys::Promise {
        let ins = self.clone();
        future_to_promise(async move {
            let params =
                js_value::json_value(params).map_err(|e| JsError::new(e.to_string().as_str()))?;
            let ret = ins
                .request_internal(method, params)
                .await
                .map_err(JsError::from)?;
            Ok(js_value::serialize(&ret).map_err(JsError::from)?)
        })
    }

    /// listen message.
    pub fn listen(&self) -> js_sys::Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            p.listen().await;
            Ok(JsValue::null())
        })
    }

    /// connect peer with remote jsonrpc server url
    pub fn connect_peer_via_http(&self, remote_url: String) -> js_sys::Promise {
        log::debug!("remote_url: {}", remote_url);
        self.request(
            "ConnectPeerViaHttp".to_string(),
            js_value::serialize(&ConnectPeerViaHttpRequest { url: remote_url }).unwrap(),
        )
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
            p.connect_with_did(did, false)
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::null())
        })
    }

    /// connect peer with web3 address, and wait for connection channel connected
    /// example:
    /// ```typescript
    /// const provider1 = new Provider()
    /// const provider2 = new Provider()
    /// const provider3 = new Provider()
    /// await create_connection(provider1, provider2);
    /// await create_connection(provider2, provider3);
    /// await provider1.connect_with_did(provider3.address())
    /// ```
    pub fn connect_with_address(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            p.connect_with_did(did, true).await.map_err(JsError::from)?;
            Ok(JsValue::null())
        })
    }

    /// list all connect peers
    pub fn list_peers(&self) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let peers = p
                .swarm
                .get_connections()
                .into_iter()
                .flat_map(|(did, conn)| {
                    JsValue::try_from(&Peer::new(did, conn.webrtc_connection_state()))
                });

            let js_array = js_sys::Array::from_iter(peers);
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
            let destination_did = get_did(destination.as_str(), AddressType::DEFAULT)?;
            p.send_message(destination_did, &msg.to_vec())
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
            match p.swarm.get_connection(did) {
                None => Ok(JsValue::null()),
                Some(conn) => {
                    let peer = Peer::new(did, conn.webrtc_connection_state());
                    Ok(JsValue::try_from(&peer)?)
                }
            }
        })
    }

    /// wait for connection connected
    /// * address: peer's address
    pub fn wait_for_connected(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        self.wait_for_data_channel_open(address, addr_type)
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
            let conn = p
                .swarm
                .get_connection(did)
                .ok_or_else(|| JsError::new("peer not found"))?;

            log::debug!("wait_for_data_channel_open start");
            if let Err(e) = conn.webrtc_wait_for_data_channel_open().await {
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
    /// - service: service name
    /// - method: http method
    /// - path: http path like `/ipfs/abc1234` `/ipns/abc`
    /// - headers: headers of request
    /// - body: body of request
    #[allow(clippy::too_many_arguments)]
    pub fn send_http_request(
        &self,
        destination: String,
        service: String,
        method: String,
        path: String,
        headers: JsValue,
        body: Option<js_sys::Uint8Array>,
        rid: Option<String>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            let destination = get_did(destination.as_str(), AddressType::DEFAULT)?;

            let method = http::Method::from_str(method.as_str())
                .map_err(JsError::from)?
                .to_string();

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

            let body = body.map(|item| item.to_vec());

            let req = HttpRequest {
                service,
                method,
                path,
                headers,
                body,
                rid,
            };

            let tx_id = p
                .send_backend_message(destination, ServiceMessage::HttpRequest(req).into())
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
            let destination = get_did(destination.as_str(), AddressType::DEFAULT)?;
            let tx_id = p
                .send_backend_message(destination, BackendMessage::PlainText(text))
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
