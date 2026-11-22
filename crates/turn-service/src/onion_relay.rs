//! External `rings-node` `onion::udp` -> `rings-turn` [`TurnDatagramRelay`] adapter (#1292).
//!
//! Bridges [`rings_turn::TurnServer`]'s RFC 5766 front end at the entry node to `rings-node`'s
//! generic world-facing `udp` exit session (`rings_node::onion::udp::OnionUdpSession`), keeping
//! `rings-core` and `rings-node` completely free of WebRTC/TURN/STUN/ICE primitives and keeping
//! `rings-turn` independent of `rings-node`.

use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::Weak;

use async_trait::async_trait;
use bytes::Bytes;
use rings_node::error::Result as NodeResult;
use rings_node::onion::native::NativeOnionCircuitHandle;
use rings_node::onion::udp::OnionUdpBindRequest;
use rings_node::onion::udp::OnionUdpFamily;
use rings_node::onion::udp::OnionUdpSession;
use rings_node::onion::OnionLoopClass;
use rings_node::processor::Processor;
use rings_node::provider::Provider;
use rings_turn::RelayPeerPolicy;
use rings_turn::TurnAllocateError;
use rings_turn::TurnDatagramRelay;
use rings_turn::TurnDatagramRelayAllocator;
use rings_turn::TurnRelayError;
use rings_turn::TurnRelayResult;
use serde::Deserialize;
use serde::Serialize;

/// Datagram relay mode for a [`crate::TurnServiceConfig`] (#1292).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRelayMode {
    /// Single-hop local relay: bind OS UDP sockets directly on the TURN host.
    #[default]
    Local,
    /// Multi-hop onion relay: each TURN `Allocate` opens a client-sealed Sphinx `onion::udp`
    /// session to a remote `udp` exit, returning `E_exit` as `XOR-RELAYED-ADDRESS`.
    OnionUdp,
}

/// Per-allocation [`TurnDatagramRelay`] backed by a live [`OnionUdpSession`].
pub struct OnionUdpRelay {
    session: OnionUdpSession,
    peer_policy: RelayPeerPolicy,
}

impl OnionUdpRelay {
    /// Wrap an opened [`OnionUdpSession`] under `peer_policy`.
    pub const fn new(session: OnionUdpSession, peer_policy: RelayPeerPolicy) -> Self {
        Self {
            session,
            peer_policy,
        }
    }

    /// Return the relayed exit socket address `E_exit`.
    pub const fn relayed_addr(&self) -> SocketAddr {
        self.session.local_addr()
    }
}

#[async_trait]
impl TurnDatagramRelay for OnionUdpRelay {
    async fn allow_peer_ip(&self, peer_ip: IpAddr) -> TurnRelayResult<()> {
        if !self.peer_policy.is_allowed_ip(peer_ip) {
            return Err(TurnRelayError::Other(
                "peer IP denied by TURN relay policy".to_string(),
            ));
        }
        self.session
            .allow_peer_ip(peer_ip)
            .await
            .map_err(|err| TurnRelayError::Other(format!("onion UDP allow_peer_ip failed: {err}")))
    }

    async fn send_to(&self, payload: &[u8], peer: SocketAddr) -> TurnRelayResult<usize> {
        if !self.peer_policy.is_allowed_addr(peer) {
            return Err(TurnRelayError::Other(
                "peer destination address denied by TURN relay policy".to_string(),
            ));
        }
        let len = payload.len();
        self.session
            .send_to(Bytes::copy_from_slice(payload), peer)
            .await
            .map_err(|err| TurnRelayError::Other(format!("onion UDP send_to failed: {err}")))?;
        Ok(len)
    }

    async fn recv_datagram(&self, buf: &mut [u8]) -> TurnRelayResult<(usize, SocketAddr)> {
        loop {
            let (data, src_addr) =
                self.session.recv_from().await.map_err(|err| {
                    TurnRelayError::Other(format!("onion UDP session closed: {err}"))
                })?;
            if !self.peer_policy.is_allowed_addr(src_addr) {
                continue;
            }
            let copy_len = buf.len().min(data.len());
            if let (Some(dst), Some(src)) = (buf.get_mut(..copy_len), data.get(..copy_len)) {
                dst.copy_from_slice(src);
            }
            return Ok((copy_len, src_addr));
        }
    }

    fn begin_close(&self) {
        self.session.begin_close();
    }

    async fn close(&self) -> TurnRelayResult<()> {
        self.session.close().await;
        Ok(())
    }
}

struct AttachedRuntime {
    processor: Weak<Processor>,
    circuit: NativeOnionCircuitHandle,
}

/// [`TurnDatagramRelayAllocator`] opening multi-hop `rings-node` `onion::udp` exit sessions for
/// each admitted TURN allocation (#1292).
pub struct OnionUdpRelayBackend {
    runtime: RwLock<Option<AttachedRuntime>>,
    use_ipv4: bool,
    peer_policy: RelayPeerPolicy,
    loop_class: OnionLoopClass,
}

impl OnionUdpRelayBackend {
    /// Construct an [`OnionUdpRelayBackend`] bound immediately to `provider`'s onion runtime.
    pub fn from_provider(
        provider: &Provider,
        use_ipv4: bool,
        peer_policy: RelayPeerPolicy,
        loop_class: OnionLoopClass,
    ) -> NodeResult<Arc<Self>> {
        let backend = Self::deferred(use_ipv4, peer_policy, loop_class);
        backend.attach_provider(provider)?;
        Ok(backend)
    }

    /// Construct a deferred [`OnionUdpRelayBackend`] whose [`Provider`] will be attached during
    /// daemon startup ([`Self::attach_provider`]). Until attached, `allocate_relay` fails closed.
    pub fn deferred(
        use_ipv4: bool,
        peer_policy: RelayPeerPolicy,
        loop_class: OnionLoopClass,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime: RwLock::new(None),
            use_ipv4,
            peer_policy,
            loop_class,
        })
    }

    /// Attach `provider`'s [`Processor`] (weakly) and [`NativeOnionCircuitHandle`] so subsequent
    /// TURN `Allocate` requests open multi-hop `onion::udp` sessions over the overlay.
    pub fn attach_provider(&self, provider: &Provider) -> NodeResult<()> {
        let circuit = provider.onion_circuit()?;
        let mut slot = self.runtime.write().unwrap_or_else(PoisonError::into_inner);
        *slot = Some(AttachedRuntime {
            processor: Arc::downgrade(provider.processor()),
            circuit,
        });
        Ok(())
    }

    /// Configured Sphinx loop class (`16 KiB` by default).
    pub const fn loop_class(&self) -> OnionLoopClass {
        self.loop_class
    }
}

#[async_trait]
impl TurnDatagramRelayAllocator for OnionUdpRelayBackend {
    fn is_ipv4(&self) -> bool {
        self.use_ipv4
    }

    async fn allocate_relay(
        &self,
        use_ipv4: bool,
        need_even_port: bool,
    ) -> std::result::Result<(Arc<dyn TurnDatagramRelay>, SocketAddr), TurnAllocateError> {
        if use_ipv4 != self.use_ipv4 {
            return Err(TurnAllocateError::ErrInvalidRequestedFamilyValue);
        }
        let (processor, circuit) = {
            let guard = self.runtime.read().unwrap_or_else(PoisonError::into_inner);
            let Some(attached) = guard.as_ref() else {
                return Err(TurnAllocateError::Other(
                    "onion UDP runtime not attached yet".to_string(),
                ));
            };
            let Some(processor) = attached.processor.upgrade() else {
                return Err(TurnAllocateError::Other(
                    "onion UDP node processor stopped".to_string(),
                ));
            };
            (processor, attached.circuit.clone())
        };

        let bind_request = OnionUdpBindRequest {
            family: if use_ipv4 {
                OnionUdpFamily::Ipv4
            } else {
                OnionUdpFamily::Ipv6
            },
            even_port: need_even_port,
        };
        let session = circuit
            .open_udp_session_with_class(&processor, bind_request, self.loop_class)
            .await
            .map_err(|err| {
                TurnAllocateError::Other(format!("failed to open onion UDP session: {err}"))
            })?;
        let relayed_addr = session.local_addr();
        let relay = Arc::new(OnionUdpRelay::new(session, self.peer_policy));
        Ok((relay, relayed_addr))
    }
}
