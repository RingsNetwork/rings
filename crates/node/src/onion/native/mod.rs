//! Native composition of the onion circuit adapters.
//!
//! A native node installs one circuit protocol whose handler composes two adapters over shared
//! node-wide resources (exit accounting, link outbox, forward-replay witness):
//!
//! ```text
//! exit frame     ──https service?──▶ HTTPS exit ──handled──▶ done
//!                        │ otherwise
//!                        └────────────────────────────────▶ TCP exit
//! backward frame ──claim(peer, id)──▶ HTTPS client ──Some(claim)──▶ claim.resolve(payload)
//!                                        │ None
//!                                        └──────────────────────▶ TCP client streams
//! ```
//!
//! Law (client disjointness): the HTTPS client and the TCP streams draw circuit ids independently
//! and uniformly from 128 bits, and the HTTPS client claims only the pair `(circuit id, expected
//! return peer)` it registered. A TCP payload is therefore misdelivered only if both allocators
//! drew the same id for routes with the same first hop, with probability at most `n² / 2¹²⁹` for
//! `n` live circuits — the same unguessability every edge-local circuit id already relies on.

use std::sync::Arc;

use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::message::MessageSigner;
use tokio::net::TcpStream;

use crate::error::Result;
use crate::extension::ext::Extensions;
use crate::extension::ext::Scope;
use crate::onion::circuit::OnionAuthenticatedPayload;
use crate::onion::circuit::OnionCircuitCapabilities;
use crate::onion::circuit::OnionCircuitExitFrame;
use crate::onion::circuit::OnionCircuitHandler;
use crate::onion::circuit::OnionCircuitId;
use crate::onion::circuit::OnionCircuitProtocol;
use crate::onion::circuit::OnionCircuitShell;
use crate::onion::circuit::OnionLinkSender;
use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::https::try_handle_https_exit_payload;
use crate::onion::https::OnionHttpsCall;
use crate::onion::https::OnionHttpsClient;
use crate::onion::https::OnionHttpsResponse;
use crate::onion::https::OnionHttpsRuntime;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::proxy::ONION_PROXY_HTTPS_SERVICE;
use crate::onion::replay::OnionForwardReplayWitness;
use crate::onion::tcp::NativeOnionOpenStream;
use crate::onion::tcp::NativeOnionTcpExitConfig;
use crate::onion::tcp::OnionTcpRuntime;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion::OnionServiceName;

/// Native handle for TCP streams and HTTPS requests over route-aware onion circuits.
#[derive(Clone)]
pub struct NativeOnionCircuitHandle {
    tcp: Arc<OnionTcpRuntime>,
    https: Arc<OnionHttpsClient>,
    scope: Scope,
}

impl NativeOnionCircuitHandle {
    /// Install the route-aware onion circuit protocol.
    pub fn install(
        extensions: &Extensions,
        delegatee_key: DelegateeKey,
        network_id: u32,
        allow_relay: bool,
        exit_config: Option<NativeOnionTcpExitConfig>,
    ) -> Result<Self> {
        let exit_epoch = exit_config
            .as_ref()
            .map(|_| extensions.core().onion_exit_epoch());
        let (tcp, https) = native_onion_runtimes(delegatee_key.clone(), network_id, exit_config);
        if let Some(config) = tcp.exit_config() {
            if config.allows_service(&OnionServiceName::https()) {
                https.set_exit_policy(Some(config.policy().clone()));
                https.set_native_proxy(config.https_proxy().map(ToString::to_string));
            }
        }
        let capabilities = OnionCircuitCapabilities::from_registration(allow_relay, exit_epoch);
        let signer = MessageSigner::new(delegatee_key.clone(), network_id);
        extensions.register(
            OnionCircuitProtocol::new(capabilities),
            OnionCircuitShell::with_link_sender(
                delegatee_key,
                NativeOnionCircuitHandler::new(Arc::clone(&tcp), Arc::clone(&https), signer),
                tcp.link_sender().clone(),
            ),
        )?;
        Ok(Self {
            tcp,
            https: Arc::clone(https.client()),
            scope: Scope::new(extensions.core(), ONION_CIRCUIT_NAMESPACE.to_string()),
        })
    }

    /// Relay an already-accepted TCP stream over `route`.
    pub async fn relay_tcp_stream(
        &self,
        stream: TcpStream,
        route: OnionRoute,
        target: OnionProxyTarget,
    ) -> Result<()> {
        let opened = self.open_tcp_stream(route, target).await?;
        opened.relay(stream);
        Ok(())
    }

    /// Open a TCP stream over `route` and wait until the exit has connected the target.
    pub async fn open_tcp_stream(
        &self,
        route: OnionRoute,
        target: OnionProxyTarget,
    ) -> Result<NativeOnionOpenStream> {
        self.tcp
            .open_client_connection(self.scope.clone(), route, target)
            .await
    }

    /// Send `call` to `route.target` over `route` and wait for the exit's authenticated response.
    ///
    /// Obtain the target and call from [`OnionHttpsCall::from_url`], then select `route` for that
    /// target under
    /// [`OnionProxyConfig::https_proxy`](crate::onion::proxy::OnionProxyConfig::https_proxy).
    /// Dropping the future cancels the pending circuit, and a silent exit yields
    /// [`Error::OnionProxyRequestTimedOut`](crate::error::Error::OnionProxyRequestTimedOut).
    pub async fn request_https(
        &self,
        route: &OnionProxyRoute,
        call: OnionHttpsCall,
    ) -> Result<OnionHttpsResponse> {
        self.https.request(self.scope.clone(), route, call).await
    }
}

/// Build the TCP and HTTPS adapters over one set of node-wide exit and link resources.
pub(super) fn native_onion_runtimes(
    delegatee_key: DelegateeKey,
    network_id: u32,
    exit_config: Option<NativeOnionTcpExitConfig>,
) -> (Arc<OnionTcpRuntime>, Arc<OnionHttpsRuntime>) {
    let accounting = OnionExitAccounting::default();
    let link_sender = OnionLinkSender::default();
    let forward_replays = OnionForwardReplayWitness::default();
    let return_key = delegatee_key.delegatee_public_key();
    let tcp = Arc::new(OnionTcpRuntime::with_resources(
        delegatee_key,
        network_id,
        exit_config,
        accounting.clone(),
        link_sender.clone(),
        forward_replays.clone(),
    ));
    let https = Arc::new(OnionHttpsRuntime::with_resources(
        return_key,
        accounting,
        link_sender,
        forward_replays,
    ));
    (tcp, https)
}

/// Circuit handler dispatching each frame to the HTTPS or TCP adapter (see the module diagram).
#[derive(Clone)]
pub(super) struct NativeOnionCircuitHandler {
    tcp: Arc<OnionTcpRuntime>,
    https: Arc<OnionHttpsRuntime>,
    /// The exit's signing authority for backward payloads.
    signer: MessageSigner<DelegateeKey>,
}

impl NativeOnionCircuitHandler {
    /// Compose both adapters under one exit signing authority.
    pub(super) const fn new(
        tcp: Arc<OnionTcpRuntime>,
        https: Arc<OnionHttpsRuntime>,
        signer: MessageSigner<DelegateeKey>,
    ) -> Self {
        Self { tcp, https, signer }
    }
}

#[async_trait::async_trait]
impl OnionCircuitHandler for NativeOnionCircuitHandler {
    async fn handle_exit(&self, scope: &Scope, frame: OnionCircuitExitFrame) -> Result<()> {
        if frame.payload.matches_service(ONION_PROXY_HTTPS_SERVICE)
            && try_handle_https_exit_payload(
                &self.https,
                self.signer.by_ref(),
                scope,
                frame.clone(),
            )
            .await?
        {
            return Ok(());
        }
        self.tcp.handle_exit_payload(scope.clone(), frame).await
    }

    async fn handle_client(
        &self,
        _scope: &Scope,
        from: Did,
        circuit_id: OnionCircuitId,
        payload: OnionAuthenticatedPayload,
    ) -> Result<()> {
        match self.https.client().claim(from, circuit_id)? {
            Some(claim) => {
                claim.resolve(payload, self.signer.network_id());
                Ok(())
            }
            None => {
                self.tcp
                    .handle_client_payload(from, circuit_id, payload)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests;
