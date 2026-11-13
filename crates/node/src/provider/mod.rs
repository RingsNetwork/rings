//! General Provider, this module provide Provider implementation for FFI and WASM

use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use rings_core::delegation::DelegateeKey;
use rings_core::delegation::DelegationBuilder;
use rings_core::dht::Did;
use rings_core::dht::EntryStorage;
use rings_core::dht::RemovalStorage;
use rings_core::ecc::PublicKey;
use rings_core::lifecycle::StopToken;
use rings_core::measure::EvidenceCounters;
use rings_core::measure::EvidenceDigest;
use rings_core::measure::EvidenceError;
use rings_core::measure::EvidencePage;
use rings_core::measure::PeerMeasurement;
use rings_core::message::ReplayStorage;
use rings_core::storage::MemStorage;
use rings_core::swarm::application::ApplicationData;
use rings_core::swarm::application::ApplicationMessage;
use rings_rpc::protos::rings_node_handler::InternalRpcHandler;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;
use crate::extension::Backend;
use crate::extension::BackendObserver;
use crate::extension::DispatchDropCounters;
use crate::extension::DispatchDrops;
use crate::mail::registration::MailOffer;
use crate::measure::EvidenceCollectorIdentity;
use crate::measure::EvidenceStorage;
use crate::measure::MeasureStorage;
use crate::measure::PeriodicMeasure;
use crate::measure::UnavailableEvidenceStorage;
#[cfg(feature = "node")]
use crate::onion::native::NativeOnionCircuitHandle;
use crate::onion::runtime::OnionRuntime;
use crate::onion::OnionEntryGuardStorage;
use crate::prelude::wasm_export;
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
use crate::processor::DelegationHorizon;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;
use crate::release::StoresReleased;

const CLOSE_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(all(feature = "browser", target_family = "wasm"))]
pub mod browser;

async fn await_close_phase<F>(
    phase_fut: F,
    timeout: std::time::Duration,
    phase: &'static str,
) -> bool
where
    F: Future<Output = ()>,
{
    let deadline = rings_runtime::sleep(timeout);
    futures::pin_mut!(phase_fut, deadline);
    match futures::future::select(phase_fut, deadline).await {
        futures::future::Either::Left(((), _)) => true,
        futures::future::Either::Right((timer, _)) => {
            match timer {
                Ok(()) => tracing::warn!(
                    phase,
                    timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    "provider close timed out waiting for shutdown phase"
                ),
                Err(error) => tracing::warn!(
                    phase,
                    ?error,
                    "provider close timer unavailable while waiting for shutdown phase"
                ),
            }
            false
        }
    }
}
#[cfg(feature = "ffi")]
pub mod ffi;

/// Maximum E2E events retained in a provider's bounded [`E2eInbox`] before further events drop.
const E2E_EVENT_CAPACITY: usize = 4096;

/// One inbound E2E handshake or stream-frame event retained by [`Provider`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct E2eEvent {
    /// Event kind (`"handshakeRequest"`, `"handshakeResponse"`, or `"streamFrame"`).
    pub kind: String,
    /// Verified sender DID.
    pub from: String,
    /// Sender / responder compressed secp256k1 public key when present.
    pub public_key: Option<String>,
    /// Per-frame ECIES ephemeral secp256k1 public key for `"streamFrame"` events.
    pub ephemeral_public_key: Option<String>,
    /// Stream UUID for `"streamFrame"` events.
    pub stream_id: Option<String>,
    /// Stream sequence number for `"streamFrame"` events.
    pub sequence: Option<u64>,
    /// Whether this frame terminates the stream.
    pub is_final: Option<bool>,
    /// Legacy block count reported for `"streamFrame"` events.
    pub ciphertext_blocks: Option<usize>,
    /// Base64-encoded ciphertext payload for `"streamFrame"` events.
    pub ciphertext: Option<String>,
}

/// Snapshot returned when draining retained E2E events from [`Provider`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TakeE2eEventsResponse {
    /// Events retained since the previous drain.
    pub events: Vec<E2eEvent>,
    /// Cumulative events dropped since provider creation because the bounded inbox was full.
    pub dropped: u64,
}

/// JSON/JS-safe snapshot of this node's active delegation signing horizon.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SigningHorizonSnapshot {
    /// Delegation expiry timestamp in Unix epoch milliseconds (decimal string).
    pub expires_at_ms: String,
    /// First millisecond at which outbound frames can no longer be signed (decimal string).
    pub horizon_ms: String,
    /// Milliseconds remaining before `horizon_ms` at snapshot time (decimal string).
    pub remaining_ms: String,
    /// Whether this node can sign outbound frames at snapshot time.
    pub signs_now: bool,
}

#[derive(Default)]
pub(crate) struct E2eInbox {
    events: Mutex<Vec<E2eEvent>>,
    dropped: AtomicU64,
}

impl E2eInbox {
    pub(crate) fn take(&self) -> Result<TakeE2eEventsResponse> {
        let mut events = self.events.lock().map_err(|_| Error::Lock)?;
        Ok(TakeE2eEventsResponse {
            events: std::mem::take(&mut *events),
            dropped: self.dropped.load(Ordering::Relaxed),
        })
    }

    fn push(&self, event: E2eEvent) -> Result<()> {
        let mut events = self.events.lock().map_err(|_| Error::Lock)?;
        if events.len() < E2E_EVENT_CAPACITY {
            events.push(event);
        } else {
            let dropped = self
                .dropped
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            tracing::debug!(dropped, "provider E2E event inbox full; event dropped");
        }
        Ok(())
    }
}

impl BackendObserver for E2eInbox {
    fn e2e_message(&self, message: &ApplicationMessage) {
        match e2e_event_from_message(message)
            .and_then(|event| event.map_or(Ok(()), |e| self.push(e)))
        {
            Ok(()) => {}
            Err(error) => tracing::warn!(error = ?error, "provider E2E event not retained"),
        }
    }
}

struct CompositeBackendObserver {
    e2e_inbox: Arc<E2eInbox>,
    extra: Option<Arc<dyn BackendObserver>>,
}

impl BackendObserver for CompositeBackendObserver {
    fn link_fact(&self, fact: crate::extension::ext::LinkFact) {
        if let Some(extra) = self.extra.as_deref() {
            extra.link_fact(fact);
        }
    }

    fn e2e_message(&self, message: &ApplicationMessage) {
        self.e2e_inbox.e2e_message(message);
        if let Some(extra) = self.extra.as_deref() {
            extra.e2e_message(message);
        }
    }
}

fn e2e_event_from_message(message: &ApplicationMessage) -> Result<Option<E2eEvent>> {
    let from = message.from.to_string();
    Ok(Some(match &message.data {
        ApplicationData::E2eHandshakeRequest(request) => E2eEvent {
            kind: "handshakeRequest".to_owned(),
            from,
            public_key: Some(public_key_json_string(request.requester_public_key)?),
            ephemeral_public_key: None,
            stream_id: None,
            sequence: None,
            is_final: None,
            ciphertext_blocks: None,
            ciphertext: None,
        },
        ApplicationData::E2eHandshakeResponse(response) => E2eEvent {
            kind: "handshakeResponse".to_owned(),
            from,
            public_key: Some(public_key_json_string(response.responder_public_key)?),
            ephemeral_public_key: None,
            stream_id: None,
            sequence: None,
            is_final: None,
            ciphertext_blocks: None,
            ciphertext: None,
        },
        ApplicationData::E2eStreamFrame(frame) => E2eEvent {
            kind: "streamFrame".to_owned(),
            from,
            public_key: Some(public_key_json_string(frame.sender_public_key)?),
            ephemeral_public_key: Some(public_key_json_string(frame.ephemeral_public_key)?),
            stream_id: Some(frame.stream_id.to_string()),
            sequence: Some(frame.sequence),
            is_final: Some(frame.is_final),
            ciphertext_blocks: Some(frame.ciphertext_blocks()),
            ciphertext: Some(base64::encode(&frame.ciphertext)),
        },
        _ => return Ok(None),
    }))
}

fn public_key_json_string(public_key: PublicKey<33>) -> Result<String> {
    let value = serde_json::to_value(public_key)?;
    value.as_str().map(str::to_owned).ok_or(Error::InvalidData)
}

/// General Provider holding the node processor, RPC dispatcher, and extension registry.
#[derive(Clone)]
#[allow(dead_code)]
#[wasm_export]
pub struct Provider {
    processor: Arc<Processor>,
    handler: InternalRpcHandler,
    extensions: crate::extension::ext::Extensions,
    dispatch_drops: Arc<DispatchDrops>,
    e2e_inbox: Arc<E2eInbox>,
    /// The onion runtime installed in this provider's registry, shared by its clones; its lock
    /// serializes the check-and-install, so the runtime is installed at most once.
    onion_runtime: Arc<Mutex<Option<OnionRuntime>>>,
    #[cfg(all(feature = "browser", target_family = "wasm"))]
    onion_directory_endpoint: Arc<Mutex<Option<BrowserDirectoryEndpoint>>>,
    /// The retained browser inbox and its recipient, shared by provider clones. Law: only a
    /// constructor that opts in before processor build installs this state.
    #[cfg(all(feature = "browser", target_family = "wasm"))]
    browser_mail: Option<std::rc::Rc<browser::provider::mail::BrowserMail>>,
    /// Best-effort `pagehide` departure listener for browser providers (#1057).
    #[cfg(all(feature = "browser", target_family = "wasm"))]
    pagehide: Option<std::rc::Rc<browser::provider::listener::PagehideListener>>,
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
#[derive(Clone)]
pub(crate) struct BrowserDirectoryEndpoint {
    pub(crate) url: String,
    pub(crate) api_token: Option<Arc<str>>,
}

/// Async signer, without Send required
#[cfg(all(feature = "browser", target_family = "wasm"))]
pub type AsyncSigner = Box<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>>>>>;

/// Async signer, use for non-wasm envirement, Send is necessary
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
pub type AsyncSigner = Box<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send>>>;

/// Signer can be async and sync
#[allow(clippy::type_complexity)]
pub enum Signer {
    /// Sync signer
    Sync(Box<dyn Fn(String) -> Result<Vec<u8>>>),
    /// Async signer
    Async(AsyncSigner),
}

/// The stores a provider is built over, each optional: an absent one takes the default its
/// builder documents (an in-memory store, or none where the law admits no volatile one).
#[derive(Default)]
pub(crate) struct ProviderStores {
    /// The DHT data store (element sides).
    pub(crate) entry: Option<EntryStorage>,
    /// The relay-inbox store.
    pub(crate) relay: Option<EntryStorage>,
    /// The store of the data namespace's remove sides (#920).
    pub(crate) removals: Option<RemovalStorage>,
    /// The peer measurement store.
    pub(crate) measure: Option<MeasureStorage>,
    /// The provisional-evidence store.
    pub(crate) evidence: Option<EvidenceStorage>,
    /// The onion entry-guard store.
    pub(crate) guards: Option<OnionEntryGuardStorage>,
    /// The transaction replay store.
    pub(crate) replay: Option<ReplayStorage>,
    /// The release witnesses of the file stores above, none for a store with no directory.
    pub(crate) released: StoresReleased,
}

#[allow(dead_code)]
impl Provider {
    /// Create provider from processor directly
    pub fn from_processor(processor: Arc<Processor>) -> Self {
        let extensions = crate::extension::ext::Extensions::new(processor.clone());
        #[cfg(all(feature = "browser", target_family = "wasm"))]
        let pagehide = browser::provider::listener::PagehideListener::register(&processor)
            .map(std::rc::Rc::new);
        Self {
            processor,
            handler: InternalRpcHandler,
            extensions,
            dispatch_drops: Arc::default(),
            e2e_inbox: Arc::default(),
            onion_runtime: Arc::new(Mutex::new(None)),
            #[cfg(all(feature = "browser", target_family = "wasm"))]
            onion_directory_endpoint: Arc::new(Mutex::new(None)),
            #[cfg(all(feature = "browser", target_family = "wasm"))]
            browser_mail: None,
            #[cfg(all(feature = "browser", target_family = "wasm"))]
            pagehide,
        }
    }

    /// The shared protocol registry. The [`Backend`] serving core's application inbox clones
    /// this, so registration (via the provider) and dispatch see the same table.
    pub fn extensions(&self) -> crate::extension::ext::Extensions {
        self.extensions.clone()
    }

    /// This provider's onion runtime, installed in its registry on first use: the data plane
    /// with the processor's role, and for an exit the worlds of its offer the platform
    /// interprets (`onion::runtime`).
    ///
    /// Idempotent within this provider's registry, which its clones share: the check and the
    /// install run under the slot's lock, so concurrent callers install once and share the
    /// runtime. Another provider over the same processor has its own registry.
    ///
    /// # Errors
    ///
    /// [`Error::Lock`] for a poisoned slot, or the failure of [`OnionRuntime::install`].
    pub(crate) fn onion_runtime(&self) -> Result<OnionRuntime> {
        let mut slot = self.onion_runtime.lock().map_err(|_| Error::Lock)?;
        if let Some(runtime) = slot.as_ref() {
            return Ok(runtime.clone());
        }
        let runtime = OnionRuntime::install(&self.extensions())?;
        *slot = Some(runtime.clone());
        Ok(runtime)
    }

    /// Install the onion runtime when the processor's role registers `relay`, so a node never
    /// advertises a relay it does not run; a runtime installed before is kept.
    ///
    /// # Errors
    ///
    /// As for [`Self::onion_runtime`].
    pub(crate) fn install_relay_onion_runtime(&self) -> Result<()> {
        if self.processor.onion_role().registers_relay() {
            self.onion_runtime().map(drop)
        } else {
            Ok(())
        }
    }

    /// Cross-platform handle for registering intermediate `invoke` backends and evaluating
    /// multi-stage Kleisli pipelines on this provider's onion runtime (#834 Phase 2b).
    pub fn onion_pipeline(&self) -> Result<crate::onion::OnionPipelineHandle> {
        self.onion_runtime()
            .map(crate::onion::OnionPipelineHandle::new)
    }

    /// The drop counters the [`Backend`] serving this provider's application inbox records in.
    pub(crate) fn dispatch_drops(&self) -> Arc<DispatchDrops> {
        Arc::clone(&self.dispatch_drops)
    }

    /// The application envelopes the [`Backend`] dropped instead of dispatching, by reason.
    pub fn dispatch_drop_counters(&self) -> DispatchDropCounters {
        self.dispatch_drops.counters()
    }

    pub(crate) fn take_e2e_events_internal(&self) -> Result<TakeE2eEventsResponse> {
        self.e2e_inbox.take()
    }

    /// Drain all retained inbound E2E handshake and stream-frame events.
    #[cfg(not(all(feature = "browser", target_family = "wasm")))]
    pub fn take_e2e_events(&self) -> Result<TakeE2eEventsResponse> {
        self.take_e2e_events_internal()
    }

    pub(crate) fn signing_horizon_snapshot(&self) -> SigningHorizonSnapshot {
        let now_ms = self.processor.clock().now_ms();
        let horizon = self.processor.signing_horizon();
        SigningHorizonSnapshot {
            expires_at_ms: horizon.expires_at_ms().to_string(),
            horizon_ms: horizon.horizon_ms().to_string(),
            remaining_ms: horizon.horizon_ms().saturating_sub(now_ms).to_string(),
            signs_now: horizon.signs_at(now_ms),
        }
    }

    /// Return the active delegation signing horizon of this node.
    #[cfg(not(all(feature = "browser", target_family = "wasm")))]
    pub fn signing_horizon(&self) -> DelegationHorizon {
        self.processor.signing_horizon()
    }

    /// Renew this node's active session key in process before its signing horizon expires.
    #[cfg(not(all(feature = "browser", target_family = "wasm")))]
    pub fn renew_delegatee_key(&self, next: DelegateeKey) -> Result<DelegationHorizon> {
        self.processor
            .renew_delegatee_key(next, self.processor.clock().now_ms())
    }

    /// The capability handle — overlay `send` / `did` / self-addressed `inject`. (Authenticated
    /// `dispatch` is router-only; `pub(crate)` so it never reaches public callers.)
    pub(crate) fn core(&self) -> crate::extension::ext::Core {
        self.extensions.core()
    }

    /// Register a pure [`Protocol`](crate::extension::ext::Protocol) together with its
    /// [`Interpret`](crate::extension::ext::Interpret) shell under the protocol's namespace.
    /// Errors if the namespace is already taken.
    pub fn register_protocol<P, I>(&self, protocol: P, interpret: I) -> Result<()>
    where
        P: crate::extension::ext::Protocol + rings_runtime::MaybeSendSync + 'static,
        P::State: rings_runtime::MaybeSendSync + 'static,
        P::Effect: rings_runtime::MaybeSendSync,
        I: crate::extension::ext::Interpret<Effect = P::Effect>
            + rings_runtime::MaybeSendSync
            + 'static,
    {
        self.extensions.register(protocol, interpret)
    }

    /// Send a namespaced payload to a peer. This is the uniform upper-layer send — a core
    /// capability, identical on native and browser.
    pub async fn send(
        &self,
        to: rings_core::dht::Did,
        namespace: &str,
        payload: bytes::Bytes,
    ) -> Result<()> {
        self.core().send(to, namespace, payload).await
    }

    /// Return local measurement counters for a peer, if observed.
    pub async fn peer_measurement(&self, did: Did) -> Option<PeerMeasurement> {
        self.processor.peer_measurement(did).await
    }

    /// Return every retained local peer measurement.
    pub async fn peer_measurements(&self) -> Vec<PeerMeasurement> {
        self.processor.peer_measurements().await
    }

    /// Return one bounded page of canonical provisional service-receipt evidence.
    pub async fn provisional_evidence_page(
        &self,
        after: Option<EvidenceDigest>,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<EvidencePage<Did>, EvidenceError> {
        self.processor.provisional_evidence_page(after, limit).await
    }

    /// Return aggregate provisional-evidence counters without account labels.
    pub async fn provisional_evidence_counters(&self) -> EvidenceCounters {
        self.processor.provisional_evidence_counters().await
    }

    /// Return aggregate final-destination origin-quota drops by lane and reason.
    pub fn origin_quota_counters(&self) -> rings_core::message::OriginQuotaCounters {
        self.processor.origin_quota_counters()
    }

    pub(crate) async fn flush_measurements(&self) -> Result<()> {
        self.processor.flush_measurements().await
    }

    /// Resolves once every registration of this node is stored at its owners, as they reported
    /// it ([`Processor::registrations_published`], #866).
    pub async fn registrations_published(&self) {
        self.processor.registrations_published().await
    }

    pub(crate) async fn leave_internal(&self) -> Result<()> {
        self.processor.leave().await
    }

    /// Best-effort voluntary departure (#1057): hand off live relay carriers to remaining
    /// members and send `Leave { successor_list }` to this node's predecessor and successor head
    /// before transports close.
    #[cfg(not(all(feature = "browser", target_family = "wasm")))]
    pub async fn leave(&self) -> Result<()> {
        self.leave_internal().await
    }

    /// Close this node and release every store it holds, so that the stores can be opened
    /// again in this process: an in-process re-init (#1085).
    ///
    /// Law (close): the steps run in this order, each once the one before it has ended.
    ///
    /// ```text
    ///   (a) stop     every node task is told to end; every listener generation ends
    ///   (b) drain    no node task is live (the one-spawn law of `crate::tasks`)
    ///   (c) network  every connection is closed, and no new one is admitted (#1098)
    ///   (d) flush    the measurements are persisted
    ///   (e) release  no node task is live again; the last clone of the node drops, and with
    ///                it every store
    /// ```
    ///
    /// Steps (c) and (d) are [`Processor::close`]. A node task spawned after (b), during (c) or
    /// (d) (by a callback of the closing network, say), starts with the stop already fired: it
    /// ends at its first suspension point, or winds down within its bound. (e) drains again
    /// before the release, so no such task holds the node into it.
    ///
    /// Post: `Ok` means every store the node was built over is released and its directory
    /// unlocked (the release law of `rings_core::storage::file`), so an open of those stores in
    /// this process succeeds. `Err(Error::ProviderStillShared { released })` means another clone
    /// of the node, a `Provider` or the [`Processor`] it shares, is still held. Steps (a) to (d)
    /// have run, so the node is stopped and its network closed. Its stores are released only
    /// when the last clone drops, and `released` resolves then. Any other `Err` is the first
    /// failure of step (c) or (d); step (e) has run all the same, so the stores are released.
    /// On the shared path a failure of (c) or (d) is logged, since `released` is what the
    /// caller needs next.
    ///
    /// Bound: no step waits on a timer of its own: each is woken by the end it awaits. (a) and
    /// (b) last one suspension of each node task. A task winding down at the stop (a relay
    /// resetting its socket and sending its session's close) adds a send, bounded by the send
    /// timeout, and a listener generation adds its measurement flush. (c) is the swarm's close
    /// of its connections. (e) also awaits the tasks the core detached that still hold a part
    /// of the node. Each of those either ends with the dropped node (an inbound actor, the
    /// outbound workers, whose channels close with it), is bounded by its own timeout (a
    /// handshake, a payload send), or is a replay transition or flush, which ends after its
    /// write.
    pub async fn close(self) -> Result<()> {
        self.close_with_drain_timeout(CLOSE_DRAIN_TIMEOUT).await
    }

    pub(crate) async fn close_with_drain_timeout(
        self,
        drain_timeout: std::time::Duration,
    ) -> Result<()> {
        let tasks = self.processor.tasks().clone();
        tasks.stop();
        await_close_phase(
            self.processor.await_listener_generations(),
            drain_timeout,
            "listener_generations",
        )
        .await;
        await_close_phase(tasks.drained(), drain_timeout, "tasks_before_network_close").await;
        let closed = self.processor.close_with_timeout(drain_timeout).await;
        await_close_phase(tasks.drained(), drain_timeout, "tasks_after_network_close").await;
        let released = self.processor.stores_released();
        let Self {
            processor,
            handler,
            extensions,
            dispatch_drops,
            e2e_inbox,
            onion_runtime,
            #[cfg(all(feature = "browser", target_family = "wasm"))]
            onion_directory_endpoint,
            #[cfg(all(feature = "browser", target_family = "wasm"))]
            browser_mail,
            #[cfg(all(feature = "browser", target_family = "wasm"))]
            pagehide,
        } = self;
        drop((
            handler,
            extensions,
            dispatch_drops,
            e2e_inbox,
            onion_runtime,
        ));
        #[cfg(all(feature = "browser", target_family = "wasm"))]
        drop((onion_directory_endpoint, browser_mail, pagehide));
        // Atomic: `Some` only for the last strong clone, which a `Weak` upgrade cannot race.
        match Arc::into_inner(processor) {
            Some(processor) => {
                drop(processor);
                if !await_close_phase(
                    std::future::IntoFuture::into_future(released.clone()),
                    drain_timeout,
                    "stores_released",
                )
                .await
                {
                    if let Err(error) = closed {
                        tracing::warn!(%error, "a closing node whose stores are still held failed to close its network");
                    }
                    return Err(Error::ProviderStillShared { released });
                }
                closed
            }
            None => {
                if let Err(error) = closed {
                    tracing::warn!(%error, "a shared node failed to close its network");
                }
                Err(Error::ProviderStillShared { released })
            }
        }
    }

    /// Create a provider instance with storage name
    pub(crate) async fn new_provider_with_storage_internal(
        config: ProcessorConfig,
        stores: ProviderStores,
    ) -> Result<Provider> {
        Self::new_provider_with_storage_and_mail_internal(config, stores, None).await
    }

    /// Build over `stores`, installing the chosen mail heartbeat before the processor exists.
    /// Law: `None` publishes no mail descriptor; an offer is already schedule-validated.
    async fn new_provider_with_storage_and_mail_internal(
        config: ProcessorConfig,
        stores: ProviderStores,
        mail_offer: Option<MailOffer>,
    ) -> Result<Provider> {
        let ProviderStores {
            entry: entry_storage,
            relay: relay_storage,
            removals: removal_storage,
            measure: measure_storage,
            evidence: evidence_storage,
            guards: onion_entry_guard_storage,
            replay: replay_storage,
            released,
        } = stores;
        let entry_storage = entry_storage.unwrap_or_else(|| Box::new(MemStorage::new()));
        let measure_storage = measure_storage.unwrap_or_else(|| Box::new(MemStorage::new()));
        let evidence_storage = evidence_storage
            .unwrap_or_else(|| Box::new(UnavailableEvidenceStorage) as EvidenceStorage);
        let onion_entry_guard_storage =
            onion_entry_guard_storage.unwrap_or_else(|| Box::new(MemStorage::new()));

        let collector = EvidenceCollectorIdentity::new(
            config.network_id(),
            config.delegatee_key().delegator_did(),
        );
        let measure = PeriodicMeasure::new_with_evidence_storage_and_clock(
            measure_storage,
            evidence_storage,
            collector,
            config.clock().clone(),
        )
        .await?;

        let processor_builder = ProcessorBuilder::from_config(&config)?
            .storage(entry_storage)
            .onion_entry_guard_storage(onion_entry_guard_storage)
            .measure(measure)
            .stores_released(released);
        let processor_builder = match replay_storage {
            Some(replay_storage) => processor_builder.replay_storage(replay_storage),
            None => processor_builder,
        };
        let processor_builder = match relay_storage {
            Some(relay_storage) => processor_builder.relay_storage(relay_storage),
            None => processor_builder,
        };
        let processor_builder = match removal_storage {
            Some(removal_storage) => processor_builder.removal_storage(removal_storage),
            None => processor_builder,
        };
        let processor_builder = match mail_offer {
            Some(offer) => processor_builder.receive_mail(offer),
            None => processor_builder,
        };

        let processor = Arc::new(processor_builder.build()?);

        Ok(Self::from_processor(processor))
    }

    /// Create a new provider instanice with everything in detail
    /// Ice_servers should obey forrmat: `"[turn|strun]://<Address>:<Port>;..."`
    /// Account is hex string
    /// Account should format as same as account_type declared
    /// Account_type is lowercase string, possible input are: `eip191`, `ed25519`, `bip137`, for more information,
    /// please check [rings_core::ecc]
    /// Signer should accept a String and returns bytes.
    /// Signer should function as same as account_type declared, Eg: eip191 or secp256k1 or ed25519.
    pub(crate) async fn new_provider_internal(
        network_id: u32,
        ice_servers: String,
        stabilize_interval: u64,
        account: String,
        account_type: String,
        signer: Signer,
        stores: ProviderStores,
    ) -> Result<Provider> {
        Self::new_provider_internal_with_config(
            network_id,
            ice_servers,
            stabilize_interval,
            account,
            account_type,
            signer,
            stores,
            core::convert::identity,
        )
        .await
    }

    pub(crate) async fn mint_delegatee_key(
        account: String,
        account_type: String,
        signer: Signer,
    ) -> Result<DelegateeKey> {
        let mut sk_builder = DelegationBuilder::new(account, account_type);
        let proof = sk_builder.unsigned_proof();
        let sig = match signer {
            Signer::Sync(s) => s(proof)?,
            Signer::Async(s) => s(proof).await?,
        };
        sk_builder = sk_builder.set_delegator_signature(sig);
        sk_builder.build().map_err(Error::InternalError)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new_provider_internal_with_config(
        network_id: u32,
        ice_servers: String,
        stabilize_interval: u64,
        account: String,
        account_type: String,
        signer: Signer,
        stores: ProviderStores,
        configure: impl FnOnce(ProcessorConfig) -> ProcessorConfig,
    ) -> Result<Provider> {
        let delegatee_key = Self::mint_delegatee_key(account, account_type, signer).await?;
        let config =
            ProcessorConfig::new(network_id, ice_servers, delegatee_key, stabilize_interval);
        let config = configure(config);
        Self::new_provider_with_storage_internal(config, stores).await
    }

    /// Serve the swarm's application inbox with the extension [`Backend`] on a detached task,
    /// so inbound custom messages are decoded as
    /// [`Envelope`](crate::extension::ext::Envelope)s and routed to their namespace's protocol.
    /// Call once, after registering every protocol whose messages may already be on their way
    /// (relay mail included): from the claim on, an envelope of a namespace not yet registered
    /// is dropped, counted. The inbox is claimed once, so a second call fails with
    /// `ApplicationInboxClaimed`. Serving lasts until the node is closed ([`Provider::close`]):
    /// the service is a node task (#1085). Use
    /// [`application_service`](Provider::application_service) for one under a stop of the
    /// caller's.
    pub fn set_backend(&self) -> Result<()> {
        self.serve_application(None)
    }

    /// [`Self::set_backend`] with `observer` handed the peer facts and unrouted E2E messages.
    pub(crate) fn serve_application(
        &self,
        observer: Option<Arc<dyn BackendObserver>>,
    ) -> Result<()> {
        let tasks = self.processor.tasks();
        // Acquired before the inbox is claimed, so a missing runtime claims nothing.
        let spawner = tasks.spawner().map_err(|_| {
            Error::ExtensionError("no runtime to serve the application inbox".to_owned())
        })?;
        let service = self.application_service(observer, tasks.stop_token())?;
        spawner.spawn(service);
        Ok(())
    }

    /// Claim the swarm's application inbox now and return the future that serves it with the
    /// extension [`Backend`] over this provider's protocol registry, handing peer facts and
    /// unrouted E2E messages to `observer`, until the swarm is gone or `stop` is requested.
    ///
    /// ```text
    ///   application_service(o, s) = claim inbox  ;  pure (serve_o(inbox, s))
    ///   serve_o                    = Backend::new(self).observed_by(o).serve
    /// ```
    ///
    /// The claim is eager, so data that arrives before the caller spawns the future waits in
    /// the inbox instead of being discarded unclaimed; the caller owns only the spawner and the
    /// stop token. As with [`set_backend`](Provider::set_backend), register protocols first:
    /// an envelope of a namespace not yet registered when it is read is dropped, counted. The
    /// inbox is claimed once (see
    /// [`Swarm::application_inbox`](rings_core::swarm::Swarm::application_inbox)), so a second
    /// call fails.
    pub fn application_service(
        &self,
        observer: Option<Arc<dyn BackendObserver>>,
        stop: StopToken,
    ) -> Result<impl Future<Output = ()> + 'static> {
        let inbox = self
            .processor
            .swarm
            .application_inbox()
            .map_err(Error::InternalError)?;
        let composite_observer: Arc<dyn BackendObserver> = Arc::new(CompositeBackendObserver {
            e2e_inbox: Arc::clone(&self.e2e_inbox),
            extra: observer,
        });
        let backend = Backend::new(Arc::new(self.clone())).observed_by(composite_observer);
        Ok(backend.serve(inbox, stop))
    }

    /// Request local rpc interface
    /// the internal rpc interface is provide by rings_rpc
    pub async fn request_internal(
        &self,
        method: String,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        tracing::debug!("request {}", method);
        match method.as_str() {
            "takeE2eEvents" => return Ok(serde_json::to_value(self.take_e2e_events_internal()?)?),
            "dispatchDropCounters" => {
                return Ok(serde_json::to_value(self.dispatch_drop_counters())?);
            }
            "signingHorizon" => return Ok(serde_json::to_value(self.signing_horizon_snapshot())?),
            "renewDelegateeKey" => {
                let raw_key = params
                    .get("delegatee_key")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        Error::InternalRpcError(jsonrpc_core::Error::invalid_params(
                            "missing string field `delegatee_key`",
                        ))
                    })?;
                let next = DelegateeKey::from_str(raw_key)?;
                self.processor
                    .renew_delegatee_key(next, self.processor.clock().now_ms())?;
                return Ok(serde_json::to_value(self.signing_horizon_snapshot())?);
            }
            _ => {}
        }
        #[cfg(all(feature = "browser", target_family = "wasm"))]
        let onion_directory_endpoint = onion_directory_endpoint_from_rpc(method.as_str(), &params);
        let result = self
            .handler
            .handle_request(self.processor.clone(), method, params)
            .await
            .map_err(Error::InternalRpcError)?;
        #[cfg(all(feature = "browser", target_family = "wasm"))]
        if let Some(endpoint) = onion_directory_endpoint {
            self.set_onion_directory_endpoint(Some(endpoint))?;
        }
        Ok(result)
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

    /// Listen for messages until this future is dropped or aborted.
    ///
    /// This is a long-running task; do not await completion as a readiness signal. Listening
    /// publishes the processor's registrations, so a role that registers `relay` first installs
    /// its onion runtime (`install_relay_onion_runtime`).
    pub async fn listen(&self) -> Result<()> {
        self.install_relay_onion_runtime()?;
        self.processor.listen().await;
        Ok(())
    }

    /// Listen for messages until `stop` requests cooperative shutdown.
    ///
    /// This is a long-running task; do not await completion as a readiness signal. Like
    /// [`Self::listen`], it first installs the onion runtime of a role that registers `relay`.
    pub async fn listen_with(&self, stop: StopToken) -> Result<()> {
        self.install_relay_onion_runtime()?;
        self.processor.listen_with(stop).await;
        Ok(())
    }

    /// The native handle of this provider's onion runtime. The runtime is installed in this
    /// provider's registry on first use, once for all its clones, and shared afterwards.
    ///
    /// # Errors
    ///
    /// [`Error::Lock`] for a poisoned runtime slot, or a registry or runtime failure of the
    /// install.
    pub fn onion_circuit(&self) -> Result<NativeOnionCircuitHandle> {
        self.onion_runtime().map(NativeOnionCircuitHandle::new)
    }
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
fn onion_directory_endpoint_from_rpc(
    method: &str,
    params: &serde_json::Value,
) -> Option<BrowserDirectoryEndpoint> {
    if !matches!(method, "connectPeerViaHttp" | "ConnectPeerViaHttp") {
        return None;
    }
    let url = params
        .get("url")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(ToOwned::to_owned)?;
    let api_token = params
        .get("api_token")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(Arc::<str>::from)?;
    Some(BrowserDirectoryEndpoint {
        url,
        api_token: Some(api_token),
    })
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
impl Provider {
    pub(crate) fn set_onion_directory_endpoint(
        &self,
        endpoint: Option<BrowserDirectoryEndpoint>,
    ) -> Result<()> {
        let mut slot = self
            .onion_directory_endpoint
            .lock()
            .map_err(|_| Error::Lock)?;
        *slot = endpoint;
        Ok(())
    }

    pub(crate) fn onion_directory_endpoint(&self) -> Result<Option<BrowserDirectoryEndpoint>> {
        self.onion_directory_endpoint
            .lock()
            .map(|slot| slot.clone())
            .map_err(|_| Error::Lock)
    }
}
