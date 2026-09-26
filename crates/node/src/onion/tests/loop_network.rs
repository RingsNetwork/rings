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
use rings_core::message::MessageSigner;
use rings_core::utils::get_epoch_ms;

use crate::error::Error;
use crate::error::Result;
use crate::extension::Backend;
use crate::onion::circuit::OnionAlgebra;
use crate::onion::circuit::OnionLinkSender;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::runtime::OnionRuntime;
use crate::onion::session::serve::OnionWorld;
use crate::onion::session::serve::OnionWorldReader;
use crate::onion::session::serve::OnionWorldWriter;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionExitOffer;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionLoop;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRole;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteHop;
use crate::onion::OnionServiceName;
use crate::online::OnlineNodeType;
use crate::processor::Processor;
use crate::provider::Provider;
use crate::tests::native::network_test_guard;
use crate::tests::native::prepare_processor_with_onion_role;

/// The bound on a failing test's wait for one event.
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
        Backend::new(provider).install()?;
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
        let now_ms = get_epoch_ms();
        let network_id = self.processor.swarm.network_id();
        OnionExitDescriptor::new_signed(
            OnionExitDescriptorBody {
                did: self.did(),
                public_key: self
                    .processor
                    .swarm
                    .delegator_verification_pubkey()
                    .map_err(Error::CoreError)?,
                delegatee_public_key: self.processor.delegatee_key().delegatee_public_key(),
                process_epoch: self.processor.onion_process_epoch().get(),
                node_type: OnlineNodeType::Native,
                network_id,
                service,
                policy,
                started_at_ms: now_ms,
                heartbeat_at_ms: now_ms,
                expires_at_ms: now_ms + 90_000,
                version: crate::util::build_version(),
            },
            MessageSigner::new(self.processor.delegatee_key(), network_id),
        )
        .map_err(Error::CoreError)
    }
}

/// Link `a` and `b` and wait until both link tables hold the link.
///
/// Chord stabilization may already have connected (or be connecting) the pair once they share a
/// neighbour, so core's `AlreadyConnected` is the link being made, not a failure: the wait on
/// both link tables decides.
async fn link(a: &LoopNode, b: &LoopNode) -> Result<()> {
    if let Err(error) = dial(a, b).await {
        if !matches!(
            error,
            Error::CoreError(rings_core::error::Error::AlreadyConnected)
        ) {
            return Err(error);
        }
    }
    tokio::time::timeout(EVENT_BOUND, async {
        a.runtime.links().live(b.did()).await;
        b.runtime.links().live(a.did()).await;
    })
    .await
    .map_err(|_| Error::InvalidData)
}

/// One offer/answer exchange from `a` to `b`.
async fn dial(a: &LoopNode, b: &LoopNode) -> Result<()> {
    let offer = a.processor.swarm.create_offer(b.did()).await?;
    let answer = b.processor.swarm.answer_offer(offer).await?;
    a.processor.swarm.accept_answer(answer).await?;
    Ok(())
}

/// The network: `[client, g, r₀₂, h, r₁₁]`, linked along the loop.
pub(super) struct LoopNetwork {
    /// The client.
    pub(super) client: LoopNode,
    /// `[g, r₀₂, h, r₁₁]`.
    hops: [LoopNode; 4],
    /// The native WebRTC test lock, held while the network lives.
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl LoopNetwork {
    /// A network whose symbol hop `h` interprets `algebra`.
    pub(super) async fn new(
        algebra: impl FnOnce(&OnionExitAccounting, &OnionLinkSender) -> OnionAlgebra,
    ) -> Result<Self> {
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
            _serial: serial,
        })
    }

    /// The route `g, r₀₂, h, r₁₁, g` to `h`'s registration of `service`.
    pub(super) fn route(&self, service: OnionServiceName) -> Result<OnionRoute> {
        let [g, r02, h, r11] = &self.hops;
        let mut relays = [g, r02, r11].into_iter();
        let hops = OnionLoop::try_unfold(Vec::new(), h.route_hop(), |_| {
            relays
                .next()
                .map(LoopNode::route_hop)
                .ok_or(Error::InvalidData)
        })?;
        OnionRoute::new(
            service.clone(),
            hops,
            h.exit_descriptor(service, open_policy())?,
        )
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
