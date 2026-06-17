#![warn(missing_docs)]
//! General Provider, this module provide Provider implementation for FFI and WASM

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rings_core::dht::VNodeStorage;
use rings_core::session::SessionSkBuilder;
use rings_core::storage::MemStorage;
use rings_core::swarm::callback::SharedSwarmCallback;
use rings_rpc::protos::rings_node_handler::InternalRpcHandler;

use crate::backend::Backend;
use crate::error::Error;
use crate::error::Result;
use crate::measure::MeasureStorage;
use crate::measure::PeriodicMeasure;
use crate::prelude::wasm_export;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

#[cfg(feature = "browser")]
pub mod browser;
#[cfg(feature = "ffi")]
pub mod ffi;

/// General Provider, which holding reference of Processor
/// Provider should be obey memory layout of CLang
/// Provider should be export for wasm-bindgen
#[derive(Clone)]
#[allow(dead_code)]
#[repr(C)]
#[wasm_export]
pub struct Provider {
    processor: Arc<Processor>,
    handler: InternalRpcHandler,
    extensions: crate::backend::ext::Extensions,
    /// Live relay endpoints, shared into each interpreter: OS sockets natively,
    /// WebTransport sessions in the browser.
    #[cfg(feature = "node")]
    transport: Arc<crate::backend::transport::engine::TransportSessions>,
    #[cfg(feature = "browser")]
    transport: Arc<crate::backend::transport::wt::WtSessions>,
}

/// Async signer, without Send required
#[cfg(feature = "browser")]
pub type AsyncSigner = Box<dyn Fn(String) -> Pin<Box<dyn Future<Output = Vec<u8>>>>>;

/// Async signer, use for non-wasm envirement, Send is necessary
#[cfg(not(feature = "browser"))]
pub type AsyncSigner = Box<dyn Fn(String) -> Pin<Box<dyn Future<Output = Vec<u8>> + Send>>>;

/// Signer can be async and sync
#[allow(clippy::type_complexity)]
pub enum Signer {
    /// Sync signer
    Sync(Box<dyn Fn(String) -> Vec<u8>>),
    /// Async signer
    Async(AsyncSigner),
}

#[allow(dead_code)]
impl Provider {
    /// Create provider from processor directly
    pub fn from_processor(processor: Arc<Processor>) -> Self {
        let extensions = crate::backend::ext::Extensions::new();
        crate::backend::protocols::register_builtins(&extensions)
            .expect("register builtins on a fresh registry");
        #[cfg(feature = "node")]
        let transport = Arc::new(crate::backend::transport::engine::TransportSessions::new());
        #[cfg(feature = "browser")]
        let transport = Arc::new(crate::backend::transport::wt::WtSessions::new());
        Self {
            processor,
            handler: InternalRpcHandler,
            extensions,
            #[cfg(any(feature = "node", feature = "browser"))]
            transport,
        }
    }

    /// The shared protocol registry. The inbound callback clones this so
    /// registration (via the provider) and dispatch see the same table.
    pub fn extensions(&self) -> crate::backend::ext::Extensions {
        self.extensions.clone()
    }

    /// The effect interpreter — the single side-effecting boundary used to run a
    /// protocol's described [`Effect`](crate::backend::ext::Effect)s.
    pub(crate) fn interpreter(&self) -> crate::backend::ext::Interpreter {
        crate::backend::ext::Interpreter::new(
            self.processor.clone(),
            self.transport.clone(),
            self.extensions.computes(),
        )
    }

    /// Register a pure [`Protocol`](crate::backend::ext::Protocol) under its namespace.
    pub fn register_protocol<P>(&self, protocol: P) -> Result<()>
    where
        P: crate::backend::ext::Protocol + crate::backend::ext::MaybeSend + 'static,
        P::State: crate::backend::ext::MaybeSend + 'static,
    {
        self.extensions.register(protocol)
    }

    /// Register an impure compute job for `namespace`, run when a protocol emits an
    /// [`Effect::Compute`](crate::backend::ext::Effect::Compute). The escape hatch for
    /// effectful protocols (e.g. SNARK) whose `step` stays pure.
    pub fn register_compute(
        &self,
        namespace: impl Into<String>,
        job: crate::backend::ext::ComputeFn,
    ) -> Result<()> {
        self.extensions.register_compute(namespace, job)
    }

    /// Register (at runtime) a local service the TCP relay may dial.
    pub async fn register_tcp_service(
        &self,
        name: String,
        addr: std::net::SocketAddr,
    ) -> Result<()> {
        self.register_relay_service(crate::backend::protocols::relay::TCP, name, addr)
            .await
    }

    /// Register (at runtime) a local service the UDP relay may dial.
    pub async fn register_udp_service(
        &self,
        name: String,
        addr: std::net::SocketAddr,
    ) -> Result<()> {
        self.register_relay_service(crate::backend::protocols::relay::UDP, name, addr)
            .await
    }

    /// Register (at runtime) a WebTransport-backed service for the browser relay,
    /// mapping `name` → WebTransport `url` (under the `tcp` namespace).
    #[cfg(feature = "browser")]
    pub async fn register_wt_service(&self, name: String, url: String) -> Result<()> {
        let command = crate::backend::protocols::relay::WtCommand::RegisterService { name, url };
        let payload = bincode::serialize(&command).map_err(|_| Error::EncodeError)?;
        let envelope = crate::backend::ext::Envelope::new(
            crate::backend::protocols::relay::TCP,
            bytes::Bytes::from(payload),
        );
        self.extensions
            .dispatch(&self.interpreter(), self.processor.did(), envelope)
            .await
    }

    /// Map a service `name` → `addr` in a relay's registry, by re-injecting a local
    /// command into the relay protocol's pure `step` (provenance = self).
    async fn register_relay_service(
        &self,
        namespace: &str,
        name: String,
        addr: std::net::SocketAddr,
    ) -> Result<()> {
        let command = crate::backend::protocols::relay::Command::RegisterService { name, addr };
        let payload = bincode::serialize(&command).map_err(|_| Error::EncodeError)?;
        let envelope = crate::backend::ext::Envelope::new(namespace, bytes::Bytes::from(payload));
        self.extensions
            .dispatch(&self.interpreter(), self.processor.did(), envelope)
            .await
    }

    /// Open a local TCP tunnel: bind `local_addr` and relay each accepted connection to
    /// `peer`'s `service` (client side, forward proxy).
    pub async fn open_tcp_tunnel(
        &self,
        local_addr: std::net::SocketAddr,
        peer: rings_core::dht::Did,
        service: String,
    ) -> Result<()> {
        self.open_tunnel(
            local_addr,
            peer,
            service,
            crate::backend::protocols::relay::TCP,
            crate::backend::transport::TransportKind::Tcp,
        )
        .await
    }

    /// Open a local UDP tunnel: bind `local_addr` and relay each datagram flow to
    /// `peer`'s `service` (client side, forward proxy).
    pub async fn open_udp_tunnel(
        &self,
        local_addr: std::net::SocketAddr,
        peer: rings_core::dht::Did,
        service: String,
    ) -> Result<()> {
        self.open_tunnel(
            local_addr,
            peer,
            service,
            crate::backend::protocols::relay::UDP,
            crate::backend::transport::TransportKind::Udp,
        )
        .await
    }

    async fn open_tunnel(
        &self,
        local_addr: std::net::SocketAddr,
        peer: rings_core::dht::Did,
        service: String,
        namespace: &str,
        kind: crate::backend::transport::TransportKind,
    ) -> Result<()> {
        self.interpreter()
            .run(vec![crate::backend::ext::Effect::Listen {
                local_addr,
                peer,
                service,
                namespace: namespace.to_string(),
                kind,
            }])
            .await?;
        Ok(())
    }

    /// Send a namespaced payload to a peer. This is the uniform upper-layer send;
    /// the transport/extension plumbing underneath is identical on native and
    /// browser.
    pub async fn send(
        &self,
        to: rings_core::dht::Did,
        namespace: &str,
        payload: bytes::Bytes,
    ) -> Result<()> {
        self.interpreter()
            .run(vec![crate::backend::ext::Effect::Send {
                to,
                namespace: namespace.to_string(),
                payload,
            }])
            .await?;
        Ok(())
    }
    /// Create a provider instance with storage name
    pub(crate) async fn new_provider_with_storage_internal(
        config: ProcessorConfig,
        vnode_storage: Option<VNodeStorage>,
        measure_storage: Option<MeasureStorage>,
    ) -> Result<Provider> {
        let vnode_storage = vnode_storage.unwrap_or_else(|| Box::new(MemStorage::new()));
        let measure_storage = measure_storage.unwrap_or_else(|| Box::new(MemStorage::new()));

        let measure = PeriodicMeasure::new(measure_storage);

        let processor_builder = ProcessorBuilder::from_config(&config)?
            .storage(vnode_storage)
            .measure(measure);

        let processor = Arc::new(processor_builder.build()?);

        let extensions = crate::backend::ext::Extensions::new();
        crate::backend::protocols::register_builtins(&extensions)?;
        #[cfg(feature = "node")]
        let transport = Arc::new(crate::backend::transport::engine::TransportSessions::new());
        #[cfg(feature = "browser")]
        let transport = Arc::new(crate::backend::transport::wt::WtSessions::new());

        Ok(Provider {
            processor,
            handler: InternalRpcHandler,
            extensions,
            #[cfg(any(feature = "node", feature = "browser"))]
            transport,
        })
    }

    /// Create a new provider instanice with everything in detail
    /// Ice_servers should obey forrmat: "[turn|strun]://<Address>:<Port>;..."
    /// Account is hex string
    /// Account should format as same as account_type declared
    /// Account_type is lowercase string, possible input are: `eip191`, `ed25519`, `bip137`, for more information,
    /// please check [rings_core::ecc]
    /// Signer should accept a String and returns bytes.
    /// Signer should function as same as account_type declared, Eg: eip191 or secp256k1 or ed25519.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new_provider_internal(
        network_id: u32,
        ice_servers: String,
        stabilize_interval: u64,
        account: String,
        account_type: String,
        signer: Signer,
        vnode_storage: Option<VNodeStorage>,
        measure_storage: Option<MeasureStorage>,
    ) -> Result<Provider> {
        let mut sk_builder = SessionSkBuilder::new(account, account_type);
        let proof = sk_builder.unsigned_proof();
        let sig = match signer {
            Signer::Sync(s) => s(proof),
            Signer::Async(s) => s(proof).await,
        };
        sk_builder = sk_builder.set_session_sig(sig.to_vec());
        let session_sk = sk_builder.build().map_err(Error::InternalError)?;
        let config = ProcessorConfig::new(network_id, ice_servers, session_sk, stabilize_interval);
        Self::new_provider_with_storage_internal(config, vnode_storage, measure_storage).await
    }

    /// Install the extension [`Backend`] as the swarm's inbound callback, so inbound
    /// custom messages are decoded as [`Envelope`](crate::backend::ext::Envelope)s and
    /// routed to their namespace's protocol. Call once after registering protocols.
    pub fn set_backend(&self) -> Result<()> {
        let backend = Backend::new(Arc::new(self.clone()));
        self.processor
            .swarm
            .set_callback(Arc::new(backend))
            .map_err(Error::InternalError)
    }

    /// Set callback for swarm.
    #[deprecated(
        note = "set_swarm_callback will be removed in next version, plz use set_backend instead"
    )]
    pub fn set_swarm_callback(&self, callback: SharedSwarmCallback) -> Result<()> {
        self.processor
            .swarm
            .set_callback(callback)
            .map_err(Error::InternalError)
    }

    pub(crate) fn set_swarm_callback_internal(&self, callback: SharedSwarmCallback) -> Result<()> {
        self.processor
            .swarm
            .set_callback(callback)
            .map_err(Error::InternalError)
    }

    /// Request local rpc interface
    /// the internal rpc interface is provide by rings_rpc
    pub async fn request_internal(
        &self,
        method: String,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        tracing::debug!("request {}", method);
        self.handler
            .handle_request(self.processor.clone(), method, params)
            .await
            .map_err(Error::InternalRpcError)
    }
}

#[cfg(feature = "node")]
impl Provider {
    /// A request function implementation for native provider
    pub async fn request<T>(
        &self,
        method: rings_rpc::method::Method,
        params: T,
    ) -> Result<serde_json::Value>
    where
        T: serde::Serialize,
    {
        let params = serde_json::to_value(params)?;
        self.request_internal(method.to_string(), params).await
    }

    /// Listen messages
    pub async fn listen(&self) {
        self.processor.listen().await;
    }
}
