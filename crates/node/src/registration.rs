//! Node-layer DHT registration tasks.
//!
//! A registration task is a built-in periodic node-side publisher. The shared publisher owns
//! the heartbeat of the online-node, onion-exit and DNS-relay service registries.
//!
//! Boundedness (#918): a registry carrier is a data topic, so each descriptor expires
//! individually at its dot's issue time plus the data element horizon
//! `H = EntryKind::Data.max_lifetime_ms()` unless it is written again. A descriptor is a signed
//! lease of length `Λ` (`registry_lease`) whose bytes are fixed, so a heartbeat re-appends
//! the same bytes, which raises their dot and writes no remove, and reads nothing. A registrant
//! therefore writes no remove at all, and leaves at most `⌈H / (Λ − 2I)⌉ + 1` descriptors within
//! the horizon (13 at the defaults); the element cap keeps the newest dots, so superseded leases,
//! which no heartbeat refreshes, are the first it drops. No publisher compacts; a reset floor
//! stamped by one owner would erase every concurrent descriptor that owner never received
//! (#867).
//!
//! Ghost bound (removal-stability paper, Prop. 10): a registrant that stops at `t_s` is listed
//! until at most `t_s + T + σ` while nobody re-appends its bytes, and until at most its signed
//! lease end `ℓ ≤ t_s + Λ` against any writer or syncer.
//!
//! Trigger (#855): a registration publishes at every heartbeat, and once more at its node's
//! first admitted edge after a heartbeat that began with none (`republish_due`). Such a
//! heartbeat settled at the node alone, and the early publish routes the same lease through the
//! edge to the registry's owners. It is a heartbeat of the lease like any other, so the
//! boundedness law and the ghost bound hold as stated.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::lock::Mutex as AsyncMutex;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::dht::RelayCustody;
use rings_core::ecc::VerificationPublicKey;
use rings_core::lifecycle::StopToken;
use rings_core::message::ChordStorageInterface;
use rings_core::message::MessageSigner;
use rings_core::message::ScopedStorage;
use rings_core::swarm::storage_api::StoreReceipt;
use rings_core::utils::get_epoch_ms;
use rings_runtime::MaybeSendSync;

use crate::descriptor::heard_descriptors;
use crate::descriptor::registry_lease;
use crate::descriptor::RegistryElement;
use crate::descriptor::SignedDescriptor;
use crate::error::Error;
use crate::error::Result;
use crate::onion::OnionProcessEpochCell;
use crate::online::OnlineNodeCapabilities;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeDescriptorBody;
use crate::online::OnlineNodeType;
use crate::online::ONLINE_NODES_TOPIC;
use crate::processor::stoppable_storage_error;
use crate::processor::Processor;

const DEFAULT_ONLINE_NODE_HEARTBEAT_INTERVAL_SECS: u64 = 30;
const DEFAULT_ONLINE_NODE_TTL_SECS: u64 = 90;

/// Default online-node registry heartbeat interval in seconds.
pub(crate) const fn default_online_node_heartbeat_interval_secs() -> u64 {
    DEFAULT_ONLINE_NODE_HEARTBEAT_INTERVAL_SECS
}

/// Default online-node registry descriptor TTL in seconds.
pub(crate) const fn default_online_node_ttl_secs() -> u64 {
    DEFAULT_ONLINE_NODE_TTL_SECS
}

/// Default runtime family advertised in the online-node registry.
pub(crate) fn default_online_node_type() -> OnlineNodeType {
    #[cfg(feature = "ffi")]
    {
        OnlineNodeType::Ffi
    }
    #[cfg(all(not(feature = "ffi"), feature = "browser", target_family = "wasm"))]
    {
        OnlineNodeType::Browser
    }
    #[cfg(all(
        not(feature = "ffi"),
        not(all(feature = "browser", target_family = "wasm"))
    ))]
    {
        OnlineNodeType::Native
    }
}

/// Default node presence advertisement enablement.
pub(crate) const fn default_advertise_presence() -> bool {
    true
}

/// The heartbeat interval `I` below which a registry stays fresh: half the lease, `Λ / 2`.
///
/// Below it a lease spans more than two heartbeats, so the publisher renews it at most once per
/// `Λ − 2I` and a registrant leaves a bounded number of descriptors (the boundedness law). Each
/// append requests the data default lifetime `L = Λ` for the carrier, stamped on the publisher's
/// clock, which an owner whose clock runs up to `σ = TS_OFFSET_TOLERANCE_MS` ahead retires at
/// `L − σ`; `Λ / 2 < L − σ`, so the carrier also outlives every gap between heartbeats, a failed
/// one included (`2I < Λ`).
pub(crate) fn registry_refresh_bound() -> Duration {
    registry_lease() / 2
}

/// Whether the publish after one that began with `edges_at_start` admitted edges may come
/// before its heartbeat (#855): only after a heartbeat that began with none, which settled at
/// this node alone and reached no registry owner. An early publish arms nothing.
///
/// ```text
///   armed(heartbeat, e₀) = heartbeat ∧ e₀ = 0
/// ```
pub(crate) const fn early_publish_armed(heartbeat: bool, edges_at_start: usize) -> bool {
    heartbeat && edges_at_start == 0
}

/// Whether an armed registration publishes now, before its heartbeat (#855): it has an admitted
/// edge, through which a publish reaches the registry's owners. A first registration made before
/// the node joined the overlay is thus published again at its first edge, not one heartbeat
/// later.
///
/// Law (first edge): only a heartbeat arms the trigger and the early publish disarms it, so
/// under any sequence of admissions and retirements a registration publishes at most once
/// between two heartbeats beyond them. An early publish is an ordinary heartbeat of the lease
/// (the heartbeat law of [`DhtRegistrationPublisher`]): it raises the dots of the same bytes, so
/// the boundedness law, the freshness window `T` and the lease end `ℓ` are unchanged.
///
/// ```text
///   republish_due(armed, e) = armed ∧ e ≥ 1
/// ```
pub(crate) const fn republish_due(armed: bool, admitted_edges: usize) -> bool {
    armed && admitted_edges > 0
}

/// Validate a registry's scheduling when the switch `advertise_setting` enables it: the
/// heartbeat interval `{registry}_heartbeat_interval` is below the descriptor TTL
/// `{registry}_ttl` and below [`registry_refresh_bound`]. `registry` is the configuration-key
/// prefix of the registry the error names.
pub(crate) fn validate_registration_timing(
    registry: &str,
    advertise_setting: &str,
    advertised: bool,
    heartbeat_interval: Duration,
    ttl: Duration,
) -> Result<()> {
    if !advertised {
        return Ok(());
    }
    let keys = TimingKeys {
        heartbeat_interval: &format!("{registry}_heartbeat_interval"),
        ttl: &format!("{registry}_ttl"),
        enabled_by: Some(advertise_setting),
    };
    validate_schedule(&keys, heartbeat_interval, ttl)
}

/// The settings that schedule one registry, as its timing errors name them.
pub(crate) struct TimingKeys<'a> {
    /// The setting of the heartbeat interval.
    pub(crate) heartbeat_interval: &'a str,
    /// The setting of the descriptor time-to-live.
    pub(crate) ttl: &'a str,
    /// The switch that enables the registry, if one does.
    pub(crate) enabled_by: Option<&'a str>,
}

/// Validate a registry's scheduling: the heartbeat interval is below the descriptor TTL and
/// below [`registry_refresh_bound`], and the TTL is at most the registry lease; an error names
/// the settings in `keys`.
///
/// `I < T` keeps a registrant listed between heartbeats: a reader whose clock `t_r` runs at most
/// `σ` ahead of the publisher's `t_w` lists it while `t_r ≤ τ* + T + σ`, which holds whenever
/// `t_w − τ* ≤ T`, and `τ*` is stamped on the publisher's clock when the heartbeat is sent; the
/// heartbeat's propagation to the owners must fit in `T − I`. `T ≤ Λ` because a reader refuses
/// a longer window as not a registry's (the lease law of `descriptor`).
pub(crate) fn validate_schedule(
    keys: &TimingKeys<'_>,
    heartbeat_interval: Duration,
    ttl: Duration,
) -> Result<()> {
    let TimingKeys {
        heartbeat_interval: heartbeat_key,
        ttl: ttl_key,
        enabled_by,
    } = keys;
    if heartbeat_interval >= ttl {
        let condition = enabled_by
            .map(|switch| format!(" when {switch} is enabled"))
            .unwrap_or_default();
        return Err(Error::InvalidConfig(format!(
            "{heartbeat_key} ({heartbeat_interval:?}) must be less than {ttl_key} \
             ({ttl:?}){condition}"
        )));
    }
    let lease = registry_lease();
    if ttl > lease {
        return Err(Error::InvalidConfig(format!(
            "{ttl_key} ({ttl:?}) must be at most {lease:?}, the registry lease"
        )));
    }
    let bound = registry_refresh_bound();
    if heartbeat_interval >= bound {
        return Err(Error::InvalidConfig(format!(
            "{heartbeat_key} ({heartbeat_interval:?}) must be less than {bound:?}, half the \
             registry lease"
        )));
    }
    Ok(())
}

/// Capability passed to registration tasks.
///
/// The context exposes only the node facts and DHT publication operation that a
/// registry needs. The task does not own the processor.
pub(crate) struct RegistrationContext<'a> {
    processor: &'a Processor,
    stop: StopToken,
    /// When set, every append asks its owners for a store report (#866), and its receipt is
    /// kept here for the daemon.
    receipts: Option<Mutex<Vec<StoreReceipt>>>,
}

impl<'a> RegistrationContext<'a> {
    pub(crate) fn new(processor: &'a Processor) -> Self {
        Self::new_with_stop(processor, StopToken::never())
    }

    pub(crate) const fn new_with_stop(processor: &'a Processor, stop: StopToken) -> Self {
        Self {
            processor,
            stop,
            receipts: None,
        }
    }

    /// This context with every append asking its owners for a store report (#866).
    pub(crate) fn reporting(mut self) -> Self {
        self.receipts = Some(Mutex::new(Vec::new()));
        self
    }

    /// The receipts of the reported appends made through this context, or `None` when it was
    /// not reporting.
    pub(crate) fn into_receipts(self) -> Option<Vec<StoreReceipt>> {
        self.receipts.map(|receipts| {
            receipts
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner)
        })
    }

    /// Return whether the owning registration daemon has requested shutdown.
    pub(crate) fn should_stop(&self) -> bool {
        self.stop.should_stop()
    }

    pub(crate) fn ensure_running(&self) -> Result<()> {
        if self.should_stop() {
            return Err(Error::RegistrationStopped);
        }
        Ok(())
    }

    /// Return the local node DID.
    pub(crate) fn did(&self) -> Did {
        self.processor.did()
    }

    /// Return the local network id.
    pub(crate) fn network_id(&self) -> u32 {
        self.processor.swarm.network_id()
    }

    /// Return storage redundancy for the local DHT protocol mode.
    pub(crate) fn storage_redundancy(&self) -> u16 {
        self.processor.swarm.storage_redundancy()
    }

    /// Return storage virtual-node positions for the local DHT protocol mode.
    pub(crate) fn dht_virtual_nodes(&self) -> u16 {
        self.processor.swarm.dht_virtual_nodes()
    }

    /// Return the relay custody the local node declares: whether it holds relay mail.
    pub(crate) fn relay_custody(&self) -> RelayCustody {
        self.processor.swarm.dht().relay_custody()
    }

    /// Return the account verification public key.
    pub(crate) fn delegator_verification_pubkey(&self) -> Result<VerificationPublicKey> {
        self.processor
            .swarm
            .delegator_verification_pubkey()
            .map_err(Error::CoreError)
    }

    /// Return the local session signing key.
    pub(crate) fn delegatee_key(&self) -> &DelegateeKey {
        self.processor.delegatee_key()
    }

    /// The authority that signs this node's descriptors: its delegatee key inside its overlay.
    pub(crate) fn message_signer(&self) -> MessageSigner<&DelegateeKey> {
        MessageSigner::new(self.delegatee_key(), self.network_id())
    }

    /// The DHT operations of this registration: their rerouting waits end at its stop.
    fn storage(&self) -> ScopedStorage<'_> {
        self.processor.swarm.scoped_storage(self.stop.clone())
    }

    /// Append `data` to `topic`, stopping cooperatively with the registration; a reporting
    /// context keeps the append's store receipt.
    pub(crate) async fn append_data(&self, topic: &str, data: Bytes) -> Result<()> {
        let Some(receipts) = &self.receipts else {
            return self
                .storage()
                .storage_append_data(topic, data)
                .await
                .map_err(stoppable_storage_error);
        };
        let receipt = self
            .storage()
            .storage_append_data_reported(topic, data)
            .await
            .map_err(stoppable_storage_error)?;
        receipts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(receipt);
        Ok(())
    }
}

/// The terms of a registry lease: the signed lease end `ℓ` and freshness window `T` every
/// descriptor of the lease carries (see the lease law of `descriptor`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LeaseTerms {
    /// Freshness window `T` in milliseconds.
    pub(crate) fresh_for_ms: u64,
    /// Lease end `ℓ` in milliseconds since Unix epoch.
    pub(crate) expires_at_ms: u128,
}

/// The descriptors one registration publishes under a lease: one (`[D; 1]`) or one per service
/// (`Vec<D>`), so a caller gets back exactly the shape it built.
///
/// Law: `heard_at(t)` maps [`SignedDescriptor::heard_at`] over the descriptors and keeps their
/// number and order.
pub(crate) trait LeaseDescriptors: Clone {
    /// The descriptor type.
    type Descriptor: SignedDescriptor + RegistryElement;

    /// The descriptors, in publication order.
    fn as_slice(&self) -> &[Self::Descriptor];

    /// The descriptors as heard at `heartbeat_at_ms`.
    fn heard_at(self, heartbeat_at_ms: u128) -> Self;
}

impl<D> LeaseDescriptors for [D; 1]
where D: SignedDescriptor + RegistryElement + Clone
{
    type Descriptor = D;

    fn as_slice(&self) -> &[D] {
        self
    }

    fn heard_at(self, heartbeat_at_ms: u128) -> Self {
        self.map(|descriptor| descriptor.heard_at(heartbeat_at_ms))
    }
}

impl<D> LeaseDescriptors for Vec<D>
where D: SignedDescriptor + RegistryElement + Clone
{
    type Descriptor = D;

    fn as_slice(&self) -> &[D] {
        self
    }

    fn heard_at(self, heartbeat_at_ms: u128) -> Self {
        self.into_iter()
            .map(|descriptor| descriptor.heard_at(heartbeat_at_ms))
            .collect()
    }
}

/// The lease a publisher holds: its terms, the signed bodies it was issued over, its descriptors,
/// and the elements every heartbeat of the lease re-appends.
#[derive(Clone, Debug)]
struct HeldLease<S> {
    terms: LeaseTerms,
    bodies: Vec<Vec<u8>>,
    descriptors: S,
    elements: Vec<Bytes>,
}

/// Common publisher for DHT-backed registries: the heartbeat of a signed lease (#918;
/// removal-stability paper, Law 8).
///
/// Law (heartbeat): a heartbeat at `t` appends exactly the current lease's elements, the same
/// bytes every time, so it raises their dots and writes no remove; it reads nothing. It issues a
/// new lease, `ℓ' = t + Λ`, only when `t + 2I ≥ ℓ` or when a signed field changed, and appends
/// the new lease's elements instead; it never removes the old ones, which readers outrank by
/// their lease and the element horizon retires.
///
/// ```text
///   heartbeat(t):
///     held ∧ t + 2I < ℓ ∧ bodies(ℓ) = held.bodies ─▶ Extend(held.elements)
///     otherwise ─▶ held ← sign(bodies(t + Λ)); Extend(held.elements)
/// ```
#[derive(Clone, Debug)]
pub(crate) struct DhtRegistrationPublisher<S> {
    topic: String,
    /// The renewal margin `2I`: a lease is renewed once fewer than two heartbeats remain in it.
    renewal_margin_ms: u128,
    /// The freshness window `T` this registration signs.
    fresh_for_ms: u64,
    /// The lease held, if any; its lock also serializes the heartbeats.
    held: Arc<AsyncMutex<Option<HeldLease<S>>>>,
}

impl<S: LeaseDescriptors> DhtRegistrationPublisher<S> {
    /// A publisher for `topic` whose heartbeat runs every `heartbeat_interval` and whose
    /// descriptors are fresh for `fresh_for`.
    pub(crate) fn new(
        topic: impl Into<String>,
        heartbeat_interval: Duration,
        fresh_for: Duration,
    ) -> Self {
        Self {
            topic: topic.into(),
            renewal_margin_ms: heartbeat_interval.as_millis().saturating_mul(2),
            fresh_for_ms: u64::try_from(fresh_for.as_millis()).unwrap_or(u64::MAX),
            held: Arc::new(AsyncMutex::new(None)),
        }
    }

    /// Publish one heartbeat at `now_ms` of the lease whose descriptors `descriptors_for` signs
    /// under given terms (the heartbeat law), returning them as heard now.
    ///
    /// Each append is rerouted as `Processor::storage_fetch` documents, and its rerouting wait
    /// ends at the registration's stop.
    pub(crate) async fn publish_lease(
        &self,
        context: &RegistrationContext<'_>,
        now_ms: u128,
        descriptors_for: impl Fn(LeaseTerms) -> Result<S>,
    ) -> Result<S> {
        let mut held = self.held.lock().await;
        context.ensure_running()?;
        let kept = match held.as_ref() {
            Some(lease)
                if now_ms.saturating_add(self.renewal_margin_ms) < lease.terms.expires_at_ms =>
            {
                let bodies = signing_data(descriptors_for(lease.terms)?.as_slice())?;
                (bodies == lease.bodies).then(|| lease.clone())
            }
            _ => None,
        };
        let lease = match kept {
            Some(lease) => lease,
            None => {
                let terms = LeaseTerms {
                    fresh_for_ms: self.fresh_for_ms,
                    expires_at_ms: now_ms.saturating_add(registry_lease().as_millis()),
                };
                let descriptors = descriptors_for(terms)?;
                let lease = HeldLease {
                    terms,
                    bodies: signing_data(descriptors.as_slice())?,
                    elements: descriptors
                        .as_slice()
                        .iter()
                        .map(|descriptor| descriptor.to_element().map_err(Error::CoreError))
                        .collect::<Result<_>>()?,
                    descriptors,
                };
                *held = Some(lease.clone());
                lease
            }
        };
        for element in &lease.elements {
            context.ensure_running()?;
            context.append_data(&self.topic, element.clone()).await?;
        }
        Ok(lease.descriptors.heard_at(now_ms))
    }
}

/// The signed bodies of `descriptors`, which decide whether a lease still describes them.
fn signing_data<D: SignedDescriptor>(descriptors: &[D]) -> Result<Vec<Vec<u8>>> {
    descriptors
        .iter()
        .map(|descriptor| {
            descriptor
                .descriptor_signing_data()
                .map_err(Error::CoreError)
        })
        .collect()
}

/// Periodic node-layer registration.
#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
pub(crate) trait RegistrationTask: MaybeSendSync {
    /// Stable name used in logs.
    fn name(&self) -> &'static str;

    /// Time between registration attempts.
    fn interval(&self) -> Duration;

    /// Publish one registration heartbeat.
    async fn register_once(&self, context: &RegistrationContext<'_>) -> Result<()>;
}

/// Online-node registry task.
#[derive(Clone, Debug)]
pub struct OnlineNodeRegistration {
    heartbeat_interval: Duration,
    node_type: OnlineNodeType,
    started_at_ms: u128,
    endpoint_hint: Option<String>,
    /// The process epoch this node registers `relay` at, if it relays (#834 D2): read at every
    /// heartbeat, so a renewed epoch is published by the next one.
    onion_relay: Option<OnionProcessEpochCell>,
    publisher: DhtRegistrationPublisher<[OnlineNodeDescriptor; 1]>,
}

impl OnlineNodeRegistration {
    /// Create an online-node registration task.
    pub fn new(
        heartbeat_interval: Duration,
        ttl: Duration,
        node_type: OnlineNodeType,
        endpoint_hint: Option<String>,
        onion_relay: Option<OnionProcessEpochCell>,
    ) -> Self {
        Self {
            heartbeat_interval,
            node_type,
            started_at_ms: get_epoch_ms(),
            endpoint_hint,
            onion_relay,
            publisher: DhtRegistrationPublisher::new(ONLINE_NODES_TOPIC, heartbeat_interval, ttl),
        }
    }

    /// Build this node's signed descriptor under the lease `terms`.
    pub(crate) fn descriptor_at(
        &self,
        context: &RegistrationContext<'_>,
        terms: LeaseTerms,
    ) -> Result<OnlineNodeDescriptor> {
        OnlineNodeDescriptor::new_signed(
            OnlineNodeDescriptorBody {
                did: context.did(),
                public_key: context.delegator_verification_pubkey()?,
                delegatee_public_key: context.delegatee_key().delegatee_public_key(),
                node_type: self.node_type.clone(),
                network_id: context.network_id(),
                storage_redundancy: context.storage_redundancy(),
                dht_virtual_nodes: context.dht_virtual_nodes(),
                capabilities: OnlineNodeCapabilities {
                    onion_relay: self.onion_relay.as_ref().map(OnionProcessEpochCell::get),
                    relay_custody: context.relay_custody(),
                },
                endpoint_hint: self.endpoint_hint.clone(),
                started_at_ms: self.started_at_ms,
                fresh_for_ms: terms.fresh_for_ms,
                expires_at_ms: terms.expires_at_ms,
                version: crate::util::build_version(),
            },
            context.message_signer(),
        )
        .map_err(Error::CoreError)
    }

    /// Publish one heartbeat of this node's signed online descriptor, returning it as heard
    /// now.
    pub(crate) async fn publish_descriptor(
        &self,
        context: &RegistrationContext<'_>,
    ) -> Result<OnlineNodeDescriptor> {
        let [descriptor] = self
            .publisher
            .publish_lease(context, get_epoch_ms(), |terms| {
                self.descriptor_at(context, terms)
                    .map(|descriptor| [descriptor])
            })
            .await?;
        Ok(descriptor)
    }

    /// Decode online-node descriptors from a DHT entry, each heard at its element's dot.
    pub fn descriptors_from_entry(
        entry: &rings_core::dht::entry::Entry,
    ) -> Vec<OnlineNodeDescriptor> {
        heard_descriptors(entry)
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl RegistrationTask for OnlineNodeRegistration {
    fn name(&self) -> &'static str {
        "online-node"
    }

    fn interval(&self) -> Duration {
        self.heartbeat_interval
    }

    async fn register_once(&self, context: &RegistrationContext<'_>) -> Result<()> {
        self.publish_descriptor(context).await.map(|_| ())
    }
}

mod readiness;

pub(crate) use readiness::RegistrationReadiness;

#[cfg(test)]
mod test_first_edge;
