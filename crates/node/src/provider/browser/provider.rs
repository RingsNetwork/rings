//! Browser Provider implementation
#![allow(non_snake_case, non_upper_case_globals, clippy::ptr_offset_with_cast)]
use std::convert::TryFrom;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;

use js_sys;
use js_sys::Uint8Array;
use rings_core::consts::REMOVAL_STORE_MAX_REMOVES;
use rings_core::dht::entry;
use rings_core::dht::Did;
use rings_core::dht::EntryStorage;
use rings_core::dht::RemovalStorage;
use rings_core::ecc::PublicKey;
use rings_core::message::ReplayStorage;
use rings_core::message::TRANSACTION_REPLAY_STORE_MAX_RECORDS;
use rings_core::storage::RecordAuthority;
use rings_core::utils::js_value;
use rings_derive::wasm_export;
use rings_rpc::protos::rings_node::ConnectPeerViaHttpRequest;
use wasm_bindgen::JsError;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures;
use wasm_bindgen_futures::future_to_promise;
use wasm_bindgen_futures::JsFuture;

use crate::error::Error;
use crate::error::Result as NodeResult;
use crate::mail::registration::MailOffer;
use crate::measure::EvidenceStorage;
use crate::measure::MeasureStorage;
use crate::onion::https::OnionHttpsClient;
use crate::onion::https_onion_exit_services;
use crate::onion::OnionEntryGuardStorage;
use crate::onion::OnionExitOffer;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionRole;
use crate::processor::ProcessorConfig;
use crate::provider::AsyncSigner;
use crate::provider::Provider;
use crate::provider::ProviderStores;
use crate::provider::Signer;
use crate::release::StoresReleased;

/// Browser listener lifecycle handle and serialized `listen` entrypoint.
pub(crate) mod listener;
/// Exclusive browser storage ownership via the Web Locks API (#1055) and its release witness.
pub(crate) mod lock;
/// Browser mail construction, retained inbox access, and filing notifications.
pub(crate) mod mail;
/// Browser onion-proxy helpers exposed by the provider API.
mod onion_proxy;
pub use listener::ProviderListener;
pub use mail::BrowserMailError;
pub use mail::BrowserMailOptions;
pub use onion_proxy::BrowserOnionProxy;
pub use onion_proxy::BrowserOnionProxyResponse;

/// Default IndexedDB storage namespace for browser providers when none is supplied.
const DEFAULT_BROWSER_STORAGE_NAME: &str = "rings-node";

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

fn wrapped_signer(signer: js_sys::Function) -> AsyncSigner {
    Box::new(
        move |data: String| -> Pin<Box<dyn Future<Output = Vec<u8>>>> {
            let signer = signer.clone();
            Box::pin(async move {
                let signer = signer.clone();
                let promise = match signer.call1(&JsValue::NULL, &JsValue::from_str(&data)) {
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

/// Row capacity of the browser relay-inbox store: the carriers this node holds or owns. A hold
/// beyond it is refused, never evicting held mail (the budget law of a retained store).
const RELAY_STORAGE_ROWS: u32 = 1024;

/// Acquire the exclusive Web Lock `rings-node/<storage_name>` (#1055) and open the seven
/// IndexedDB stores of `storage_name` under it.
///
/// Law: if another tab or node in this origin holds `rings-node/<storage_name>`, this fails
/// closed with [`Error::BrowserStorageLocked`] before any IndexedDB database is opened.
///
/// Each store's [`RecordAuthority`] and fallback policy follow the durable-records laws:
/// - **DHT entry store** (`Disposable`, 50,000 rows): retires undecodable rows; falls back to
///   in-memory storage if IndexedDB cannot open.
/// - **Relay-inbox store** (`Retained`, [`RELAY_STORAGE_ROWS`], #1021): strict durability, no
///   eviction; if IndexedDB cannot open, returns `None` so the node holds no relay mail.
/// - **Data removal store** (`Retained`, `REMOVAL_STORE_MAX_REMOVES`, #920): strict durability,
///   no eviction; falls back to in-memory removals on open failure.
/// - **Peer measurement store** (`Disposable`, 50,000 rows): falls back to in-memory storage.
/// - **Provisional evidence store** (`Disposable`, 2 rows): required; fails closed on open error.
/// - **Onion entry-guard store** (`DurableCache`, 1,024 rows, #911): retires undecodable guards;
///   falls back to in-memory guards on open failure.
/// - **Transaction replay store** (`Authoritative`, `TRANSACTION_REPLAY_STORE_MAX_RECORDS`, #923):
///   strict durability, no eviction, never retires undecodable records; fails closed on open error.
pub(super) async fn open_browser_provider_stores(
    storage_name: &str,
) -> NodeResult<(ProviderStores, Rc<lock::BrowserStorageLock>)> {
    let (storage_lock, released) = lock::lock_browser_storage(storage_name).await?;
    let storage_lock = Rc::new(storage_lock);
    let entry_storage = lock::LockedBrowserStorage::open_or_warn(
        50000,
        storage_name,
        RecordAuthority::Disposable,
        &storage_lock,
        "browser entry IndexedDB unavailable; falling back to in-memory entry storage",
    )
    .await
    .map(|storage| Box::new(storage) as EntryStorage);
    let relay_storage = lock::LockedBrowserStorage::open_or_warn(
        RELAY_STORAGE_ROWS,
        &format!("{storage_name}/relay-inbox"),
        RecordAuthority::Retained,
        &storage_lock,
        "browser relay-inbox IndexedDB unavailable; this node will hold no relay mail",
    )
    .await
    .map(|storage| Box::new(storage) as EntryStorage);
    let removal_rows = u32::try_from(REMOVAL_STORE_MAX_REMOVES).unwrap_or(u32::MAX);
    let removal_storage = lock::LockedBrowserStorage::open_or_warn(
        removal_rows,
        &format!("{storage_name}/dht-removals"),
        RecordAuthority::Retained,
        &storage_lock,
        "browser removal IndexedDB unavailable; data-topic removes are kept in memory",
    )
    .await
    .map(|storage| Box::new(storage) as RemovalStorage);
    let measure_storage = lock::LockedBrowserStorage::open_or_warn(
        50000,
        &format!("{storage_name}/measure"),
        RecordAuthority::Disposable,
        &storage_lock,
        "browser measurement IndexedDB unavailable; falling back to in-memory measurement storage",
    )
    .await
    .map(|storage| Box::new(storage) as MeasureStorage);
    let evidence_storage = Some(
        lock::LockedBrowserStorage::open(
            2,
            &format!("{storage_name}/provisional-evidence"),
            RecordAuthority::Disposable,
            &storage_lock,
        )
        .await
        .map(|storage| Box::new(storage) as EvidenceStorage)?,
    );
    let onion_entry_guard_storage = lock::LockedBrowserStorage::open_or_warn(
        1024,
        &format!("{storage_name}/onion-entry-guards"),
        RecordAuthority::DurableCache,
        &storage_lock,
        "browser onion entry-guard IndexedDB unavailable; falling back to in-memory entry guards",
    )
    .await
    .map(|storage| Box::new(storage) as OnionEntryGuardStorage);
    let replay_name = format!("{storage_name}/transaction-replay");
    let replay_rows = u32::try_from(TRANSACTION_REPLAY_STORE_MAX_RECORDS).map_err(|_| {
        Error::BrowserStorageOpen {
            name: replay_name.clone(),
            source: rings_core::error::Error::InvalidCapacity,
        }
    })?;
    let replay_storage = lock::LockedBrowserStorage::open(
        replay_rows,
        &replay_name,
        RecordAuthority::Authoritative,
        &storage_lock,
    )
    .await
    .map(|storage| Box::new(storage) as ReplayStorage)?;

    Ok((
        ProviderStores {
            entry: entry_storage,
            relay: relay_storage,
            removals: removal_storage,
            measure: measure_storage,
            evidence: evidence_storage,
            guards: onion_entry_guard_storage,
            replay: Some(replay_storage),
            released: StoresReleased::of([released]),
        },
        storage_lock,
    ))
}

impl Provider {
    /// Create a browser provider backed by IndexedDB storage and install its default backend.
    ///
    /// This is the Rust-side constructor for browser frontends. It keeps provider ownership as the
    /// lifecycle boundary while reusing the same storage, backend, and onion-protocol setup as the
    /// wasm-exported constructors.
    pub async fn new_browser_provider_with_storage(
        config: ProcessorConfig,
        storage_name: String,
    ) -> NodeResult<Self> {
        config.validate_onion_role()?;
        let (stores, _storage_lock) = open_browser_provider_stores(&storage_name).await?;
        Self::new_browser_provider_from_stores(config, stores, None).await
    }

    /// Build a browser provider from already-opened [`ProviderStores`] and an optional mail
    /// heartbeat. Law: the offer is installed before build, and existing constructors pass `None`.
    pub(super) async fn new_browser_provider_from_stores(
        config: ProcessorConfig,
        stores: ProviderStores,
        mail_offer: Option<MailOffer>,
    ) -> NodeResult<Self> {
        let provider =
            Self::new_provider_with_storage_and_mail_internal(config, stores, mail_offer).await?;
        provider.install_relay_onion_runtime()?;
        provider.set_backend()?;
        Ok(provider)
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
        future_to_promise(async move {
            let signer = wrapped_signer(signer);
            let (stores, _storage_lock) =
                open_browser_provider_stores(DEFAULT_BROWSER_STORAGE_NAME).await?;

            let provider = Provider::new_provider_internal(
                network_id,
                ice_servers,
                stabilize_interval,
                account,
                account_type,
                Signer::Async(Box::new(signer)),
                stores,
            )
            .await?;

            provider.set_backend().map_err(JsError::from)?;

            Ok(JsValue::from(provider))
        })
    }

    /// Create a browser provider that advertises an HTTPS onion exit with explicit target policy.
    #[allow(clippy::too_many_arguments)]
    pub fn new_https_exit_instance(
        network_id: u32,
        ice_servers: String,
        stabilize_interval: u64,
        account: String,
        account_type: String,
        signer: js_sys::Function,
        allowed_targets: Vec<String>,
        denied_targets: Vec<String>,
    ) -> js_sys::Promise {
        future_to_promise(async move {
            let signer = wrapped_signer(signer);
            let policy = OnionExitPolicy::from_target_strings(allowed_targets, denied_targets)
                .map_err(JsError::from)?;
            let offer =
                OnionExitOffer::new(https_onion_exit_services(), policy).map_err(JsError::from)?;
            let (stores, _storage_lock) =
                open_browser_provider_stores(DEFAULT_BROWSER_STORAGE_NAME).await?;

            let provider = Provider::new_provider_internal_with_config(
                network_id,
                ice_servers,
                stabilize_interval,
                account,
                account_type,
                Signer::Async(Box::new(signer)),
                stores,
                move |config| config.onion_role(OnionRole::Exit(offer)),
            )
            .await?;

            provider
                .install_relay_onion_runtime()
                .map_err(JsError::from)?;
            provider.set_backend().map_err(JsError::from)?;

            Ok(JsValue::from(provider))
        })
    }

    /// Create new provider instance with serialized config (yaml/json)
    pub fn new_provider_with_serialized_config(config: String) -> js_sys::Promise {
        future_to_promise(async move {
            let cfg: ProcessorConfig = serde_saphyr::from_str(&config).map_err(JsError::from)?;
            JsFuture::from(Self::new_provider_with_config(cfg)).await
        })
    }

    /// Create a new provider instance.
    pub fn new_provider_with_config(config: ProcessorConfig) -> js_sys::Promise {
        Self::new_provider_with_storage(config, DEFAULT_BROWSER_STORAGE_NAME.to_string())
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
            let provider = Self::new_browser_provider_with_storage(config, storage_name)
                .await
                .map_err(JsError::from)?;
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

    /// connect peer with remote jsonrpc server url
    pub fn connect_peer_via_http(&self, remote_url: String) -> js_sys::Promise {
        self.connect_peer_via_http_with_token(remote_url, None)
    }

    /// Connect to a peer whose remote JSON-RPC endpoint requires a Bearer token.
    pub fn connect_peer_via_http_with_token(
        &self,
        remote_url: String,
        remote_api_token: Option<String>,
    ) -> js_sys::Promise {
        log::debug!("remote_url: {remote_url}");
        let provider = self.clone();
        future_to_promise(async move {
            let params = serde_json::to_value(ConnectPeerViaHttpRequest {
                url: remote_url.clone(),
                api_token: remote_api_token,
            })
            .map_err(JsError::from)?;
            let ret = provider
                .request_internal("connectPeerViaHttp".to_string(), params)
                .await
                .map_err(JsError::from)?;
            Ok(js_value::serialize(&ret).map_err(JsError::from)?)
        })
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

    /// Best-effort voluntary departure (#1057): hand off live relay carriers and send `Leave`
    /// to this node's predecessor and successor head.
    pub fn leave(&self) -> js_sys::Promise {
        let provider = self.clone();
        future_to_promise(async move {
            provider.leave_internal().await.map_err(JsError::from)?;
            Ok(JsValue::null())
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

    /// Create a browser adapter for the standard HTTPS-over-TCP onion proxy service.
    ///
    /// The returned proxy is not bound to a URL; call [`BrowserOnionProxy::request`] with a full
    /// `https://` URL to send through the selected exit.
    pub fn onion_https_proxy(&self) -> Result<BrowserOnionProxy, JsError> {
        let runtime = self.onion_runtime().map_err(JsError::from)?;
        Ok(BrowserOnionProxy {
            processor: self.processor.clone(),
            client: OnionHttpsClient::new(runtime),
            directory_endpoint: self.onion_directory_endpoint().map_err(JsError::from)?,
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

    /// Store data on DHT storage using the data itself as its topic.
    ///
    /// The explicit topic/value pair preserves the content-derived identity of
    /// this browser API without a separate single-string entry constructor.
    /// The stored element expires by the element lifetime rule of the DHT storage
    /// chapter (`docs/src/advanced-topic/chord.md`) unless it is stored again.
    pub fn storage_store(&self, data: String) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            // Preserve the public API's topic-equals-value storage convention.
            let entry_info = entry::Entry::try_from((data.clone(), data)).map_err(JsError::from)?;
            p.storage_store(entry_info).await.map_err(JsError::from)?;
            Ok(JsValue::null())
        })
    }

    /// lookup service did on DHT by its name
    /// - name: The name of service
    pub fn lookup_service(&self, name: String) -> js_sys::Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            let dids = p
                .fetch_text_elements(&name, 0)
                .await
                .map_err(JsError::from)?
                .into_iter()
                .map(JsValue::from)
                .collect::<js_sys::Array>();
            Ok(JsValue::from(dids))
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
