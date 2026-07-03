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
use rings_core::prelude::entry;
use rings_core::prelude::entry::Entry;
use rings_core::storage::idb::IdbStorage;
use rings_core::utils::js_utils;
use rings_core::utils::js_value;
use rings_derive::wasm_export;
use rings_rpc::protos::rings_node::*;
use wasm_bindgen;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures;
use wasm_bindgen_futures::future_to_promise;
use wasm_bindgen_futures::JsFuture;

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

/// A wrapper of Arc Ref of Provider
#[derive(Clone)]
#[wasm_export]
pub struct ProviderRef {
    inner: Arc<Provider>,
}

impl ProviderRef {
    /// get wrapped arc, this is useful for wasm case
    pub fn inner(&self) -> Arc<Provider> {
        self.inner.clone()
    }
}

#[wasm_export]
impl Provider {
    /// make provider as an As arc ref
    pub fn as_ref(&self) -> ProviderRef {
        ProviderRef {
            inner: Arc::new(self.clone()),
        }
    }
}

#[wasm_export]
impl Provider {
    /// Create new instance of Provider, return Promise
    /// Ice_servers should obey forrmat: "[turn|strun]://<Address>:<Port>;..."
    /// Account is hex string
    /// Account should format as same as account_type declared
    /// Account_type is lowercase string, possible input are: `eip191`, `ed25519`, `bip137`, for more information,
    /// please check [rings_core::ecc]
    /// Signer should be `async function (proof: string): Promise<Unit8Array>`
    /// Signer should function as same as account_type declared, Eg: eip191 or secp256k1 or ed25519.
    #[wasm_bindgen(constructor)]
    pub fn new_instance(
        network_id: u32,
        ice_servers: String,
        stabilize_interval: u64,
        account: String,
        account_type: String,
        signer: js_sys::Function,
    ) -> js_sys::Promise {
        fn wrapped_signer(signer: js_sys::Function) -> AsyncSigner {
            Box::new(
                move |data: String| -> Pin<Box<dyn Future<Output = Vec<u8>>>> {
                    let signer = signer.clone();
                    Box::pin(async move {
                        let signer = signer.clone();
                        let promise = match signer.call1(&JsValue::NULL, &JsValue::from_str(&data))
                        {
                            Ok(value) => js_sys::Promise::from(value),
                            Err(error) => {
                                tracing::error!("failed to call external JS signer: {error:?}");
                                return Vec::new();
                            }
                        };
                        let value = match JsFuture::from(promise).await {
                            Ok(value) => value,
                            Err(error) => {
                                tracing::error!("external JS signer rejected: {error:?}");
                                return Vec::new();
                            }
                        };
                        let sig: js_sys::Uint8Array = Uint8Array::from(value);
                        sig.to_vec()
                    })
                },
            )
        }

        future_to_promise(async move {
            let signer = wrapped_signer(signer);

            let entry_storage = Box::new(
                IdbStorage::new_with_cap_and_name(50000, "rings-node")
                    .await
                    .map_err(JsError::from)?,
            );

            let measure_storage = Box::new(
                IdbStorage::new_with_cap_and_name(50000, "rings-node/measure")
                    .await
                    .map_err(JsError::from)?,
            );

            let provider = Provider::new_provider_internal(
                network_id,
                ice_servers,
                stabilize_interval,
                account,
                account_type,
                Signer::Async(Box::new(signer)),
                Some(entry_storage),
                Some(measure_storage),
            )
            .await?;

            provider.set_backend().map_err(JsError::from)?;

            Ok(JsValue::from(provider))
        })
    }

    /// Create new provider instance with serialized config (yaml/json)
    pub fn new_provider_with_serialized_config(config: String) -> js_sys::Promise {
        future_to_promise(async move {
            let cfg: ProcessorConfig = serde_yaml::from_str(&config).map_err(JsError::from)?;
            JsFuture::from(Self::new_provider_with_config(cfg)).await
        })
    }

    /// Create a new provider instance.
    pub fn new_provider_with_config(config: ProcessorConfig) -> js_sys::Promise {
        Self::new_provider_with_storage(config, "rings-node".to_string())
    }

    /// get self web3 address
    #[wasm_bindgen(getter)]
    pub fn address(&self) -> String {
        self.processor.did().to_string()
    }

    ///  create new unsigned Provider
    pub fn new_provider_with_storage(
        config: ProcessorConfig,
        storage_name: String,
    ) -> js_sys::Promise {
        future_to_promise(async move {
            let entry_storage = Box::new(
                IdbStorage::new_with_cap_and_name(50000, &storage_name)
                    .await
                    .map_err(JsError::from)?,
            );

            let measure_storage = Box::new(
                IdbStorage::new_with_cap_and_name(50000, &format!("{storage_name}/measure"))
                    .await
                    .map_err(JsError::from)?,
            );

            let provider = Self::new_provider_with_storage_internal(
                config,
                Some(entry_storage),
                Some(measure_storage),
            )
            .await
            .map_err(JsError::from)?;
            provider.set_backend().map_err(JsError::from)?;
            Ok(JsValue::from(provider))
        })
    }

    /// Register a protocol handler: `provider.on(namespace, initialState, handler)`.
    ///
    /// `namespace` is the protocol namespace, `initialState` is the protocol's initial
    /// state, and `handler` is a pure transition `(ctx, event) -> { state, effects }`.
    /// The handler is bridged into the same pure model native uses; effects are run by
    /// the interpreter. The lower layer (JS vs native) is invisible — callers only ever
    /// see the provider.
    pub fn on(
        &self,
        namespace: String,
        initial_state: JsValue,
        handler: js_sys::Function,
    ) -> Result<(), JsError> {
        let protocol =
            crate::extension::protocols::js::JsProtocol::new(namespace, initial_state, handler);
        self.register_protocol(protocol, crate::extension::protocols::js::JsShell)
            .map_err(JsError::from)
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

    /// Start the long-running listener.
    ///
    /// The returned Promise is not a readiness barrier and does not resolve
    /// during normal operation.
    pub fn listen(&self) -> js_sys::Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            p.listen().await;
            Ok(JsValue::null())
        })
    }

    /// connect peer with remote jsonrpc server url
    pub fn connect_peer_via_http(&self, remote_url: String) -> js_sys::Promise {
        log::debug!("remote_url: {remote_url}");
        match js_value::serialize(&ConnectPeerViaHttpRequest { url: remote_url }) {
            Ok(request) => self.request("ConnectPeerViaHttp".to_string(), request),
            Err(error) => js_sys::Promise::reject(&JsValue::from(JsError::from(error))),
        }
    }

    /// connect peer with web3 address
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
            p.connect_with_did(did).await.map_err(JsError::from)?;
            Ok(JsValue::null())
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

    /// Get local measurement counters for a peer.
    pub fn get_peer_measurement(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            let measurement = p.peer_measurement(did).await;
            let measurement = crate::rpc_dto::optional_peer_measurement_info(measurement)
                .map_err(JsError::from)?;
            let v = js_value::serialize(&measurement).map_err(JsError::from)?;
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

    /// Send a namespaced message to a peer: `provider.send_message(did, namespace, payload)`.
    ///
    /// The payload reaches the peer's protocol registered under `namespace` (see
    /// [`Provider::on`]). This is the uniform upper-layer send, identical to native
    /// [`Provider::send`](crate::provider::Provider::send).
    pub fn send_message(
        &self,
        destination: String,
        namespace: String,
        payload: js_sys::Uint8Array,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(destination.as_str(), AddressType::DEFAULT)?;
            let envelope = crate::extension::ext::Envelope::new(namespace, payload.to_vec().into());
            let tx_id = p
                .send_envelope(did, &envelope)
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from_str(tx_id.to_string().as_str()))
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

    /// Store an entry on DHT storage
    pub fn storage_store(&self, data: String) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let entry_info = entry::Entry::try_from(data).map_err(JsError::from)?;
            p.storage_store(entry_info).await.map_err(JsError::from)?;
            Ok(JsValue::null())
        })
    }

    /// lookup service did on DHT by its name
    /// - name: The name of service
    pub fn lookup_service(&self, name: String) -> js_sys::Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            let entry_key = Entry::gen_did(&name).map_err(JsError::from)?;

            tracing::debug!("browser lookup_service storage_fetch: {}", entry_key);
            p.storage_fetch(entry_key).await.map_err(JsError::from)?;
            tracing::debug!("browser lookup_service finish storage_fetch: {}", entry_key);
            js_utils::window_sleep(500).await?;
            let result = p.storage_check_cache(entry_key).await;

            if let Some(entry) = result {
                let dids = entry
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
