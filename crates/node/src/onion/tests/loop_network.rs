//! A five-node loop network for end-to-end tests: a client and the four hops of one loop
//! `client → g → r₀₂ → h → r₁₁ → g → client`, each a processor with its own data plane, linked
//! over real WebRTC connections.
//!
//! ```text
//!   client ── g ── r₀₂ ── h ── r₁₁ ── g           the five links a loop crosses
//!   h: ⟦−⟧ chosen by the test (echo, source, fetch)   every other node: its role's algebra
//! ```
//!
//! Every wait is on an event: a link counts as up once both ends' link tables hold it
//! ([`crate::onion::circuit::OnionLinkWitness`]), never after a duration. The one timeout bounds
//! a failing test.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::channel::mpsc;
use futures::StreamExt;
use rings_core::dht::Did;
use rings_core::utils::get_epoch_ms;
use rings_test_support::with_hang_guard;

use super::fixture::session_loop;
use crate::error::Error;
use crate::error::Result;
use crate::onion::circuit::OnionAlgebra;
use crate::onion::circuit::OnionLinkSender;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::runtime::OnionRuntime;
use crate::onion::session::serve::OnionWorld;
use crate::onion::session::serve::OnionWorldReader;
use crate::onion::session::serve::OnionWorldWriter;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitOffer;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRole;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteHop;
use crate::onion::OnionServiceName;
use crate::online::OnlineNodeType;
use crate::processor::Processor;
use crate::provider::Provider;
use crate::tests::exit_descriptor::ExitDescriptorFixture;
#[cfg(not(feature = "dummy"))]
use crate::tests::native::network_test_guard;
use crate::tests::native::prepare_processor_with_onion_role;
use crate::tests::native::signal_edge;

/// The hang guard of one awaited event: a failure bound, never a pacing.
pub(super) const EVENT_BOUND: Duration = Duration::from_secs(30);

/// One node of the network: its processor and its installed data plane.
pub(super) struct LoopNode {
    /// The processor.
    processor: Arc<Processor>,
    /// The node's onion runtime.
    pub(super) runtime: OnionRuntime,
}

impl LoopNode {
    /// A node of `role` whose data plane interprets `algebra`, with a backend installed.
    async fn new(
        role: OnionRole<OnionExitOffer>,
        algebra: impl FnOnce(&OnionExitAccounting, &OnionLinkSender) -> OnionAlgebra,
    ) -> Result<Self> {
        let processor = Arc::new(prepare_processor_with_onion_role(role).await);
        let provider = Arc::new(Provider::from_processor(Arc::clone(&processor)));
        let runtime = OnionRuntime::install_with(&provider.extensions(), algebra)?;
        provider.set_backend()?;
        Ok(Self { processor, runtime })
    }

    /// The node's DID.
    pub(super) fn did(&self) -> Did {
        self.processor.did()
    }

    /// The node's route entry.
    fn route_hop(&self) -> OnionRouteHop {
        OnionRouteHop::new(
            self.did(),
            self.processor.delegatee_key().delegatee_public_key(),
            self.processor.onion_process_epoch().get(),
        )
    }

    /// The node's signed registration of `service` under `policy`, as a directory serves it.
    fn exit_descriptor(
        &self,
        service: OnionServiceName,
        policy: OnionExitPolicy,
    ) -> Result<OnionExitDescriptor> {
        ExitDescriptorFixture {
            node_type: OnlineNodeType::Native,
            ..ExitDescriptorFixture::live(&self.processor, service, policy, get_epoch_ms())
        }
        .sign()
    }
}

/// Link `a` and `b` and wait until both link tables hold the link.
///
/// The edge may form from a concurrent handshake ([`signal_edge`]): the wait on both link
/// tables decides.
async fn link(a: &LoopNode, b: &LoopNode) -> Result<()> {
    signal_edge(&a.processor, &b.processor).await?;
    with_hang_guard("the loop network's link", EVENT_BOUND, async {
        a.runtime.links().live(b.did()).await;
        b.runtime.links().live(a.did()).await;
    })
    .await;
    Ok(())
}

/// The network: `[client, g, r₀₂, h, r₁₁]`, linked along the loop.
pub(super) struct LoopNetwork {
    /// The client.
    pub(super) client: LoopNode,
    /// `[g, r₀₂, h, r₁₁]`.
    hops: [LoopNode; 4],
    /// The native WebRTC test lock, held while the network lives. A `dummy` network shares
    /// nothing with another test's network and holds none.
    #[cfg(not(feature = "dummy"))]
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl LoopNetwork {
    /// A network whose symbol hop `h` interprets `algebra`.
    pub(super) async fn new(
        algebra: impl FnOnce(&OnionExitAccounting, &OnionLinkSender) -> OnionAlgebra,
    ) -> Result<Self> {
        #[cfg(not(feature = "dummy"))]
        let serial = network_test_guard().await;
        let relay = || LoopNode::new(OnionRole::Relay, |_, _| OnionAlgebra::default());
        let client = LoopNode::new(OnionRole::Client, |_, _| OnionAlgebra::default()).await?;
        let hops = [
            relay().await?,
            relay().await?,
            LoopNode::new(OnionRole::Relay, algebra).await?,
            relay().await?,
        ];
        let [g, r02, h, r11] = &hops;
        link(&client, g).await?;
        link(g, r02).await?;
        link(r02, h).await?;
        link(h, r11).await?;
        link(r11, g).await?;
        Ok(Self {
            client,
            hops,
            #[cfg(not(feature = "dummy"))]
            _serial: serial,
        })
    }

    /// The route `g, r₀₂, h, r₁₁, g` to `h`'s registration of `service`.
    pub(super) fn route(&self, service: OnionServiceName) -> Result<OnionRoute> {
        let hops = session_loop(self.hops.each_ref().map(LoopNode::route_hop));
        let [_, _, h, _] = &self.hops;
        Ok(OnionRoute::new(
            service.clone(),
            hops,
            h.exit_descriptor(service, open_policy())?,
        ))
    }
}

/// An exit policy admitting every target.
pub(super) fn open_policy() -> OnionExitPolicy {
    OnionExitPolicy::from_target_strings(vec!["*".to_string()], Vec::new())
        .expect("the wildcard parses")
}

/// A world that echoes a session's client-to-world stream back, ending when it ends.
pub(super) struct EchoWorld;

/// The read half of an echo: what the write half wrote, in order.
pub(super) struct EchoReader {
    /// The written bytes.
    written: mpsc::UnboundedReceiver<Bytes>,
    /// The part of the last write not yet read.
    pending: Bytes,
}

/// The write half of an echo.
pub(super) struct EchoWriter(Option<mpsc::UnboundedSender<Bytes>>);

#[async_trait::async_trait]
impl OnionWorld for EchoWorld {
    type Reader = EchoReader;
    type Writer = EchoWriter;

    async fn open(&self, _: &OnionProxyTarget) -> Result<(EchoReader, EchoWriter)> {
        let (writer, written) = mpsc::unbounded();
        Ok((
            EchoReader {
                written,
                pending: Bytes::new(),
            },
            EchoWriter(Some(writer)),
        ))
    }
}

#[async_trait::async_trait]
impl OnionWorldReader for EchoReader {
    async fn read(&mut self, max: usize) -> Result<Option<Bytes>> {
        if self.pending.is_empty() {
            match self.written.next().await {
                Some(bytes) => self.pending = bytes,
                None => return Ok(None),
            }
        }
        let length = max.min(self.pending.len());
        Ok(Some(self.pending.split_to(length)))
    }
}

#[async_trait::async_trait]
impl OnionWorldWriter for EchoWriter {
    async fn write(&mut self, bytes: Bytes) -> Result<()> {
        self.0
            .as_ref()
            .and_then(|writer| writer.unbounded_send(bytes).ok())
            .ok_or(Error::InvalidData)
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.0 = None;
        Ok(())
    }
}

/// A world that serves `length` bytes at open and ignores what it is written: a download.
pub(super) struct SourceWorld(pub(super) usize);

/// The read half of a download: the bytes left.
pub(super) struct SourceReader(usize);

/// The write half of a download, which discards.
pub(super) struct SourceWriter;

#[async_trait::async_trait]
impl OnionWorld for SourceWorld {
    type Reader = SourceReader;
    type Writer = SourceWriter;

    async fn open(&self, _: &OnionProxyTarget) -> Result<(SourceReader, SourceWriter)> {
        Ok((SourceReader(self.0), SourceWriter))
    }
}

#[async_trait::async_trait]
impl OnionWorldReader for SourceReader {
    async fn read(&mut self, max: usize) -> Result<Option<Bytes>> {
        let length = max.min(self.0);
        if length == 0 {
            return Ok(None);
        }
        self.0 -= length;
        Ok(Some(Bytes::from(vec![0x5a; length])))
    }
}

#[async_trait::async_trait]
impl OnionWorldWriter for SourceWriter {
    async fn write(&mut self, _: Bytes) -> Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}
