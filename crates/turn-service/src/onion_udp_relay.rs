//! Multi-hop TURN datagram relay bridge connecting [`rings_turn::TurnDatagramRelay`] to
//! [`rings_node::onion::NativeOnionUdpSession`] (#1292 Phase B).

use std::io;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use std::sync::PoisonError;
use std::sync::Weak;
use std::time::Duration;

use async_trait::async_trait;
use rings_node::error::Error;
use rings_node::error::Result;
use rings_node::onion::native::NativeOnionCircuitHandle;
use rings_node::onion::NativeOnionUdpSession;
use rings_node::onion::OnionLoopClass;
use rings_node::onion::OnionUdpBindMode;
use rings_node::processor::Processor;
use rings_node::provider::Provider;
use rings_turn::RelayPeerPolicy;
use rings_turn::TurnDatagramRelay;
use rings_turn::TurnDatagramRelayFactory;
use rings_turn::TurnRelayCloseHandle;

/// One TURN allocation's datagram relay backed by a multi-hop `onion::udp` session.
pub struct OnionUdpRelayBackend {
    session: Arc<NativeOnionUdpSession>,
    peer_policy: RelayPeerPolicy,
}

impl OnionUdpRelayBackend {
    /// Wrap an opened [`NativeOnionUdpSession`] under `peer_policy`.
    pub fn new(session: NativeOnionUdpSession, peer_policy: RelayPeerPolicy) -> Self {
        Self {
            session: Arc::new(session),
            peer_policy,
        }
    }
}

#[async_trait]
impl TurnDatagramRelay for OnionUdpRelayBackend {
    fn relayed_addr(&self) -> SocketAddr {
        self.session.local_addr()
    }

    async fn recv_datagram(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        loop {
            let next = self
                .session
                .recv_from()
                .await
                .map_err(|e| io::Error::other(e.to_string()))?;
            let Some((payload, peer)) = next else {
                return Err(io::Error::other("onion udp session closed"));
            };
            if !self.peer_policy.is_allowed_addr(peer) {
                continue;
            }
            let take = buf.len().min(payload.len());
            if let (Some(dst), Some(src)) = (buf.get_mut(..take), payload.get(..take)) {
                dst.copy_from_slice(src);
            }
            return Ok((take, peer));
        }
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        if !self.peer_policy.is_allowed_addr(target) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "relay destination address denied by policy",
            ));
        }
        self.session
            .send_to(buf, target)
            .await
            .map_err(|e| io::Error::other(e.to_string()))
    }

    async fn allow_peer_ips(&self, peer_ips: &[IpAddr], ttl: Duration) -> io::Result<()> {
        for &ip in peer_ips {
            if !self.peer_policy.is_allowed_ip(ip) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "relay peer IP denied by policy",
                ));
            }
            self.session
                .allow_peer_ip(ip, ttl)
                .await
                .map_err(|e| io::Error::other(e.to_string()))?;
        }
        Ok(())
    }

    fn begin_close(&self) -> TurnRelayCloseHandle {
        self.session.close();
        None
    }

    async fn finish_close(&self, _handle: TurnRelayCloseHandle) {
        self.session.close();
    }
}

/// [`TurnDatagramRelayFactory`] opening a fresh multi-hop `onion::udp` session per TURN
/// allocation.
pub struct OnionUdpRelayFactory {
    runtime: SyncMutex<Option<(Weak<Processor>, NativeOnionCircuitHandle)>>,
    loop_class: OnionLoopClass,
    peer_policy: RelayPeerPolicy,
    support_ipv6: bool,
}

impl OnionUdpRelayFactory {
    /// Construct a ready factory bound weakly to `processor` and its `circuit` handle.
    pub fn new(
        processor: &Arc<Processor>,
        circuit: NativeOnionCircuitHandle,
        loop_class: OnionLoopClass,
        peer_policy: RelayPeerPolicy,
    ) -> Self {
        Self {
            runtime: SyncMutex::new(Some((Arc::downgrade(processor), circuit))),
            loop_class,
            peer_policy,
            support_ipv6: true,
        }
    }

    /// Construct a deferred factory for daemon startup before [`Provider`] is initialized; call
    /// [`Self::attach_provider`] once [`Provider`] is built.
    pub fn deferred(loop_class: OnionLoopClass, peer_policy: RelayPeerPolicy) -> Arc<Self> {
        Arc::new(Self {
            runtime: SyncMutex::new(None),
            loop_class,
            peer_policy,
            support_ipv6: true,
        })
    }

    /// Bind this factory to a live [`Provider`]'s processor and [`NativeOnionCircuitHandle`].
    pub fn attach_provider(&self, provider: &Arc<Provider>) -> Result<()> {
        let circuit = provider.onion_circuit()?;
        let weak = Arc::downgrade(provider.processor());
        let mut guard = self.runtime.lock().unwrap_or_else(PoisonError::into_inner);
        *guard = Some((weak, circuit));
        Ok(())
    }
}

#[async_trait]
impl TurnDatagramRelayFactory for OnionUdpRelayFactory {
    fn supports_family(&self, use_ipv4: bool) -> bool {
        use_ipv4 || self.support_ipv6
    }

    async fn allocate_relay(
        &self,
        use_ipv4: bool,
        need_even_port: bool,
    ) -> io::Result<Arc<dyn TurnDatagramRelay>> {
        let (weak_proc, circuit) = self
            .runtime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                io::Error::other("onion udp relay factory not yet attached to provider")
            })?;
        let processor = weak_proc
            .upgrade()
            .ok_or_else(|| io::Error::other(Error::RegistrationStopped.to_string()))?;
        let mode = match (use_ipv4, need_even_port) {
            (true, false) => OnionUdpBindMode::Ipv4AnyPort,
            (true, true) => OnionUdpBindMode::Ipv4EvenPort,
            (false, false) => OnionUdpBindMode::Ipv6AnyPort,
            (false, true) => OnionUdpBindMode::Ipv6EvenPort,
        };
        let session = circuit
            .open_udp_target_with_class(&processor, mode, self.loop_class)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, ?mode, "OnionUdpRelayFactory failed to open onion::udp session");
                io::Error::other(e.to_string())
            })?;
        Ok(Arc::new(OnionUdpRelayBackend::new(
            session,
            self.peer_policy,
        )))
    }
}
