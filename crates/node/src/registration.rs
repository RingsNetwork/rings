//! Node-layer DHT registration tasks.
//!
//! A registration task is a built-in periodic node-side publisher. The shared publisher owns
//! DHT touch/tombstone mechanics for the online-node and onion-exit registries.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures::lock::Mutex as AsyncMutex;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::entry;
use rings_core::dht::Did;
use rings_core::ecc::HashStr;
use rings_core::ecc::VerificationPublicKey;
use rings_core::lifecycle::StopToken;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::MessageSigner;
use rings_core::utils::get_epoch_ms;
use rings_runtime::MaybeSendSync;

use crate::error::Error;
use crate::error::Result;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeDescriptorBody;
use crate::online::OnlineNodeType;
use crate::online::ONLINE_NODES_TOPIC;
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

/// Validate online-node registration scheduling.
pub(crate) fn validate_online_node_registration_timing(
    advertise_presence: bool,
    heartbeat_interval: Duration,
    ttl: Duration,
) -> Result<()> {
    if advertise_presence && heartbeat_interval >= ttl {
        return Err(Error::InvalidConfig(format!(
            "online_node_heartbeat_interval ({heartbeat_interval:?}) must be less than online_node_ttl ({ttl:?}) when advertise_presence is enabled"
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
}

impl<'a> RegistrationContext<'a> {
    pub(crate) fn new(processor: &'a Processor) -> Self {
        Self::new_with_stop(processor, StopToken::never())
    }

    pub(crate) const fn new_with_stop(processor: &'a Processor, stop: StopToken) -> Self {
        Self { processor, stop }
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

    pub(crate) async fn fetch_storage_entry(&self, entry_key: Did) -> Result<Option<entry::Entry>> {
        self.processor
            .fetch_storage_entry_with_stop(entry_key, &self.stop)
            .await
    }
}

/// Common publisher for DHT-backed registries.
#[derive(Clone, Debug)]
pub(crate) struct DhtRegistrationPublisher {
    topic: String,
    publish_gate: Arc<AsyncMutex<()>>,
    published_values: Arc<Mutex<BTreeSet<Encoded>>>,
    /// The mark of the last compaction this publisher issued successfully.
    last_compaction: Arc<Mutex<Option<CompactionMark>>>,
}

impl DhtRegistrationPublisher {
    /// Create a publisher for `topic`.
    pub(crate) fn new(topic: impl Into<String>) -> Self {
        Self {
            topic: topic.into(),
            publish_gate: Arc::new(AsyncMutex::new(())),
            published_values: Arc::new(Mutex::new(BTreeSet::new())),
            last_compaction: Arc::new(Mutex::new(None)),
        }
    }

    /// Publish the current value set, tombstoning older observed values with the same registry key.
    ///
    /// Invariant: registry topics are keyed presence sets, not append-only heartbeat logs.
    /// Preservation: every observed value replaced by the current publish is tombstoned after the
    /// replacement value has been touched, while unrelated publisher keys stay joinable.
    /// This publisher never compacts, so its tombstones are not bounded (#871).
    pub(crate) async fn publish_replacing(
        &self,
        context: &RegistrationContext<'_>,
        values: impl IntoIterator<Item = Encoded>,
        replaces_observed_value: impl Fn(&Encoded) -> bool,
    ) -> Result<()> {
        let current_values = values.into_iter().collect::<BTreeSet<_>>();
        let _publish_turn = self.publish_gate.lock().await;
        context.ensure_running()?;
        let observed_values = self.observed_registry_values(context).await?;
        let stale_values = {
            let mut published_values = self.published_values.lock().map_err(|_| Error::Lock)?;
            begin_registration_publish(
                &mut published_values,
                &current_values,
                observed_values,
                replaces_observed_value,
            )
        };

        for value in &current_values {
            context.ensure_running()?;
            context
                .processor
                .storage_append_data(&self.topic, value.clone())
                .await?;
        }
        for stale_value in stale_values {
            context.ensure_running()?;
            context
                .processor
                .storage_tombstone_data(&self.topic, stale_value.clone())
                .await?;
            self.published_values
                .lock()
                .map_err(|_| Error::Lock)?
                .remove(&stale_value);
        }
        {
            let mut published_values = self.published_values.lock().map_err(|_| Error::Lock)?;
            finish_registration_publish(&mut published_values, current_values);
        }
        Ok(())
    }

    /// Publish values, tombstone stale observed registry values, and compact at the owner when
    /// this node is the compactor [`plan_compacting_registration_publish`] designates.
    ///
    /// This never sends a replacement value set computed from an observed client
    /// snapshot. Compaction is requested with only the removable payloads, so the
    /// storage owner computes the final live set from its current local entry and
    /// preserves concurrent live writes it holds.
    ///
    /// A replaced descriptor is already removed by its per-dot tombstone, so a heartbeat does
    /// not compact merely because it replaced one: a compaction stamps a reset floor that also
    /// erases every concurrent add the owner has not received (#867), so it is issued only to
    /// bound tombstone metadata, by one designated registrant per crossing.
    ///
    /// `live_registrant` maps an observed value to the DID of its registrant iff the value is a
    /// live descriptor; every other value not in `values` is tombstoned.
    pub(crate) async fn publish_many_replacing_and_compacting(
        &self,
        context: &RegistrationContext<'_>,
        values: impl IntoIterator<Item = Encoded>,
        replaces_observed_value: impl Fn(&Encoded) -> bool,
        live_registrant: impl Fn(&Encoded) -> Option<Did>,
    ) -> Result<()> {
        let current_values = values.into_iter().collect::<BTreeSet<_>>();
        let _publish_turn = self.publish_gate.lock().await;
        context.ensure_running()?;
        let observed_entry = self.observed_registry_entry(context).await?;
        let last_compaction = *self.last_compaction.lock().map_err(|_| Error::Lock)?;
        let plan = {
            let mut published_values = self.published_values.lock().map_err(|_| Error::Lock)?;
            plan_compacting_registration_publish(
                &mut published_values,
                &current_values,
                RegistryObservation {
                    entry: observed_entry,
                    local: context.did(),
                    last_compaction,
                },
                |observed| {
                    should_prune_observed_registry_value(
                        observed,
                        &replaces_observed_value,
                        &|value| live_registrant(value).is_some(),
                    )
                },
                &live_registrant,
            )?
        };

        for value in &current_values {
            context.ensure_running()?;
            context
                .processor
                .storage_append_data(&self.topic, value.clone())
                .await?;
        }
        for stale_value in plan.removals.iter() {
            context.ensure_running()?;
            context
                .processor
                .storage_tombstone_data(&self.topic, stale_value.clone())
                .await?;
            self.published_values
                .lock()
                .map_err(|_| Error::Lock)?
                .remove(stale_value);
        }
        if let Some(mark) = plan.compaction {
            context.ensure_running()?;
            context
                .processor
                .storage_compact_data(&self.topic, plan.removals)
                .await?;
            // Committed only after the effect: a failed compaction is retried on the next
            // heartbeat that still observes the same register.
            *self.last_compaction.lock().map_err(|_| Error::Lock)? = Some(mark);
        }
        {
            let mut published_values = self.published_values.lock().map_err(|_| Error::Lock)?;
            finish_registration_publish(&mut published_values, current_values);
        }
        Ok(())
    }

    async fn observed_registry_values(
        &self,
        context: &RegistrationContext<'_>,
    ) -> Result<Vec<Encoded>> {
        Ok(self
            .observed_registry_entry(context)
            .await?
            .map(|entry| entry.data)
            .unwrap_or_default())
    }

    async fn observed_registry_entry(
        &self,
        context: &RegistrationContext<'_>,
    ) -> Result<Option<entry::Entry>> {
        let entry_key = entry::Entry::gen_did(&self.topic)?;
        context.fetch_storage_entry(entry_key).await
    }
}

/// The tombstone step `T` of the online-node registry's compaction ladder.
///
/// Derivation: a data carrier keeps at most `L = EntryKind::Data.max_data_len()` visible
/// elements, one add dot each, and the carrier law in `rings_core::consts` sizes a full
/// carrier for those `L` elements plus their dot metadata. Tombstones are the only part of a
/// data carrier with no cap of their own (`EntryKind::Data.max_tombstones()` is `None`); a
/// reset floor is what prunes them. `T = L` keeps the remove side of the registry carrier at
/// the scale of its add side. The resulting bound is stated on
/// [`plan_compacting_registration_publish`].
const REGISTRY_COMPACTION_TOMBSTONE_STEP: usize = entry::EntryKind::Data.max_data_len();

/// The observed register a compaction was planned under.
///
/// Every observation between two reset floors carries the same register, so a mark names
/// one threshold crossing as its observers see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CompactionMark {
    /// The observed carrier's reset floor, `None` for a carrier never floored.
    register: Option<entry::EntryVersion>,
}

impl CompactionMark {
    /// The mark of an observed carrier.
    fn of(entry: &entry::Entry) -> Self {
        Self {
            register: entry.crdt.register,
        }
    }

    /// The ring point that orders the compactors of this crossing:
    /// `anchor = H(topic_did ‖ register)`.
    ///
    /// Every observer of one crossing derives the same anchor, while successive crossings
    /// (separated by a floor, hence by a register) rotate the designation around the ring.
    fn anchor(self, topic_did: Did) -> Result<Did> {
        let bytes = rings_codec::serialize(&(topic_did, self.register))
            .map_err(|error| Error::CoreError(rings_core::error::Error::CodecSerialize(error)))?;
        Did::try_from(HashStr::from_bytes(&bytes)).map_err(Error::CoreError)
    }
}

/// A registry publisher's view of one heartbeat: the observed carrier, its own DID, and the
/// mark of its last successful compaction.
struct RegistryObservation {
    /// The carrier as this heartbeat's read returned it; possibly a previous round's reply.
    entry: Option<entry::Entry>,
    /// This registrant's DID.
    local: Did,
    /// The mark of this publisher's last successful compaction.
    last_compaction: Option<CompactionMark>,
}

/// The rank of `local` among the observed live registrants, ordered by clockwise ring
/// distance from `anchor`; `None` when `local` holds no live descriptor in the observation.
///
/// Rank `k` means exactly `k` distinct live registrants are closer to the anchor.
fn compactor_rank(
    anchor: Did,
    local: Did,
    live_registrants: impl IntoIterator<Item = Did>,
) -> Option<usize> {
    let registrants = live_registrants.into_iter().collect::<BTreeSet<_>>();
    registrants.contains(&local).then(|| {
        registrants
            .iter()
            .filter(|registrant| Did::cmp_from_observer(anchor, **registrant, local).is_lt())
            .count()
    })
}

/// The tombstone count at which the compactor of rank `rank` compacts: `T · (rank + 1)`.
fn compaction_threshold(rank: usize) -> usize {
    REGISTRY_COMPACTION_TOMBSTONE_STEP.saturating_mul(rank.saturating_add(1))
}

/// The effects of one compacting registry heartbeat.
///
/// The shell ([`DhtRegistrationPublisher::publish_many_replacing_and_compacting`]) executes it
/// in order: append the current values, tombstone [`Self::removals`], then, iff
/// [`Self::compaction`] is present, compact with [`Self::removals`] and record the mark.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RegistrationPublishPlan {
    /// Previously published or observed payloads this heartbeat removes by per-dot tombstone.
    removals: Vec<Encoded>,
    /// The mark to record once the compaction this heartbeat issues has succeeded.
    compaction: Option<CompactionMark>,
}

/// Plan one compacting registry heartbeat: the state transition
/// `(published, observation) ↦ (published′, plan)`.
///
/// ```text
///   observation.entry ─┬─► begin_registration_publish ─► removals   (per-dot, no floor)
///                      │
///                      └─► mark = register ─► anchor = H(topic ‖ register)
///                               │                  │
///                               │      rank = |{live r : r closer to anchor than local}|
///                               ▼                  ▼
///        mark ≠ last_compaction  ∧  |tombstones| ≥ T·(rank + 1)  ─►  compaction = Some(mark)
/// ```
///
/// Laws:
/// - No heartbeat floor: a non-empty `removals` never implies a compaction on its own.
/// - Single writer per crossing: every observer of one crossing sees one register, hence one
///   anchor and one ranking; below `2T` only rank 0 compacts, and the recorded mark stops it
///   from compacting again on a stale reply that still carries the pre-floor register.
/// - Ladder: a designated compactor that is dead leaves the ranking when its descriptor
///   expires, and one that stays live but never compacts is overtaken by rank `k` at `T·(k+1)`.
///
/// Bound: with `f` live registrants that never compact, reads that lag the owner by at most
/// `λ` heartbeat intervals, and `Δ` tombstones per interval from every writer (data admission
/// has no writer authority, so `Δ` includes any peer's add-then-remove writes),
/// `|tombstones(owner)| < T·(f + 1) + λ·Δ`. A crossing, and therefore a floor that can erase
/// an add the owner has not received, can be forced by any writer until #867's causally
/// stable compaction lands.
///
/// Effect on `published`: as [`begin_registration_publish`], every current value is
/// remembered before the first await.
fn plan_compacting_registration_publish(
    published_values: &mut BTreeSet<Encoded>,
    current_values: &BTreeSet<Encoded>,
    observation: RegistryObservation,
    prunes_observed_value: impl Fn(&Encoded) -> bool,
    live_registrant: impl Fn(&Encoded) -> Option<Did>,
) -> Result<RegistrationPublishPlan> {
    let Some(entry) = observation.entry else {
        return Ok(RegistrationPublishPlan {
            removals: begin_registration_publish(
                published_values,
                current_values,
                vec![],
                prunes_observed_value,
            ),
            compaction: None,
        });
    };
    let mark = CompactionMark::of(&entry);
    let rank = compactor_rank(
        mark.anchor(entry.did)?,
        observation.local,
        entry.data.iter().filter_map(&live_registrant),
    );
    let compaction = (observation.last_compaction != Some(mark)
        && rank.is_some_and(|rank| entry.crdt.tombstones.len() >= compaction_threshold(rank)))
    .then_some(mark);
    Ok(RegistrationPublishPlan {
        removals: begin_registration_publish(
            published_values,
            current_values,
            entry.data,
            prunes_observed_value,
        ),
        compaction,
    })
}

fn should_prune_observed_registry_value(
    observed: &Encoded,
    replaces_observed_value: &impl Fn(&Encoded) -> bool,
    preserves_observed_value: &impl Fn(&Encoded) -> bool,
) -> bool {
    replaces_observed_value(observed) || !preserves_observed_value(observed)
}

fn begin_registration_publish(
    published_values: &mut BTreeSet<Encoded>,
    current_values: &BTreeSet<Encoded>,
    observed_values: Vec<Encoded>,
    replaces_observed_value: impl Fn(&Encoded) -> bool,
) -> Vec<Encoded> {
    let mut stale_values = published_values
        .iter()
        .filter(|published| !current_values.contains(*published))
        .cloned()
        .collect::<BTreeSet<_>>();
    stale_values.extend(
        observed_values
            .into_iter()
            .filter(|observed| !current_values.contains(observed))
            .filter(replaces_observed_value),
    );
    // Invariant: every value whose touch may have reached storage is remembered
    // before the first await. If the publish future is later cancelled by an
    // attempt timeout, the next attempt can still tombstone the value.
    published_values.extend(current_values.iter().cloned());
    stale_values.into_iter().collect()
}

fn finish_registration_publish(
    published_values: &mut BTreeSet<Encoded>,
    current_values: BTreeSet<Encoded>,
) {
    *published_values = current_values;
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
    ttl: Duration,
    node_type: OnlineNodeType,
    started_at_ms: u128,
    endpoint_hint: Option<String>,
    /// Immutable labels selected while the processor is built.
    capabilities: Vec<String>,
    publisher: DhtRegistrationPublisher,
}

impl OnlineNodeRegistration {
    /// Create an online-node registration task.
    pub fn new(
        heartbeat_interval: Duration,
        ttl: Duration,
        node_type: OnlineNodeType,
        endpoint_hint: Option<String>,
        capabilities: Vec<String>,
    ) -> Self {
        Self {
            heartbeat_interval,
            ttl,
            node_type,
            started_at_ms: get_epoch_ms(),
            endpoint_hint,
            capabilities,
            publisher: DhtRegistrationPublisher::new(ONLINE_NODES_TOPIC),
        }
    }

    /// Build this node's signed descriptor at `now_ms`.
    pub(crate) fn descriptor_at(
        &self,
        context: &RegistrationContext<'_>,
        now_ms: u128,
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
                capabilities: self.capabilities.clone(),
                endpoint_hint: self.endpoint_hint.clone(),
                started_at_ms: self.started_at_ms,
                heartbeat_at_ms: now_ms,
                expires_at_ms: now_ms + self.ttl.as_millis(),
                version: crate::util::build_version(),
            },
            context.message_signer(),
        )
        .map_err(Error::CoreError)
    }

    /// Publish this node's signed online descriptor.
    pub(crate) async fn publish_descriptor(
        &self,
        context: &RegistrationContext<'_>,
    ) -> Result<OnlineNodeDescriptor> {
        let now_ms = get_epoch_ms();
        let descriptor = self.descriptor_at(context, now_ms)?;
        let encoded = descriptor.encode().map_err(Error::CoreError)?;
        self.publisher
            .publish_many_replacing_and_compacting(
                context,
                std::iter::once(encoded),
                |observed| {
                    observed
                        .decode::<OnlineNodeDescriptor>()
                        .is_ok_and(|descriptor| {
                            descriptor.did == context.did()
                                || (descriptor.verify_signature(context.network_id())
                                    && descriptor.is_expired_at(now_ms))
                        })
                },
                |observed| {
                    observed
                        .decode::<OnlineNodeDescriptor>()
                        .ok()
                        .filter(|descriptor| {
                            descriptor.verify_signature(context.network_id())
                                && !descriptor.is_expired_at(now_ms)
                        })
                        .map(|descriptor| descriptor.did)
                },
            )
            .await?;
        Ok(descriptor)
    }

    /// Decode online-node descriptors from a DHT entry.
    pub fn descriptors_from_entry(
        entry: &rings_core::dht::entry::Entry,
    ) -> Vec<OnlineNodeDescriptor> {
        entry
            .data
            .iter()
            .filter_map(|value| value.decode::<OnlineNodeDescriptor>().ok())
            .collect()
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;

    use rings_core::message::Encoded;

    use super::*;

    fn encoded(value: &str) -> Encoded {
        value.into()
    }

    /// The registry topic of the heartbeat model.
    const MODEL_TOPIC: &str = "registration heartbeat model";

    /// The model's first operation instant; each operation advances it by one millisecond.
    const MODEL_EPOCH_MS: u128 = 1_700_000_000_000;

    /// Heartbeat intervals by which a registrant's plan can lag the owner: its read returns
    /// the previous round's reply, and the crossing may land just after its turn.
    const MODEL_READ_LAG_INTERVALS: usize = 2;

    /// Registrant `registrant`'s descriptor stand-in at heartbeat `heartbeat`.
    fn model_descriptor(registrant: u32, heartbeat: usize) -> Encoded {
        encoded(&format!("{registrant}:{heartbeat}"))
    }

    /// The registrant of a descriptor stand-in: the model's `live_registrant`, under which
    /// every descriptor is live.
    fn model_live_registrant(value: &Encoded) -> Option<Did> {
        value
            .value()
            .split_once(':')
            .and_then(|(prefix, _heartbeat)| prefix.parse::<u32>().ok())
            .map(Did::from)
    }

    /// A registry carrier under compacting heartbeats with lagged reads, executed against a
    /// single owner.
    ///
    /// A heartbeat plans on the snapshot its previous heartbeat read (the production read
    /// answers from the reply cached by the previous round), stores the current owner snapshot
    /// for its next heartbeat, and applies the plan's effects in the order of
    /// [`DhtRegistrationPublisher::publish_many_replacing_and_compacting`]:
    ///
    /// ```text
    ///   plan(previous snapshot) ─► snapshot ─► Extend ─► Tombstone* ─► CompactData? ─► finish
    /// ```
    ///
    /// A stalled registrant plans like every other one but never issues its compaction.
    struct HeartbeatModel {
        /// The owner's carrier.
        owner: entry::Entry,
        /// Each registrant's publisher memory.
        published: BTreeMap<u32, BTreeSet<Encoded>>,
        /// Each registrant's snapshot from its previous heartbeat.
        snapshots: BTreeMap<u32, entry::Entry>,
        /// Each registrant's last successful compaction.
        last_compactions: BTreeMap<u32, CompactionMark>,
        /// Registrants that never issue a compaction.
        stalled: BTreeSet<u32>,
        /// The next operation instant.
        now_ms: u128,
        /// Tombstone operations issued so far.
        tombstones_issued: usize,
        /// The registrants of the compactions issued so far, in order.
        compactors: Vec<u32>,
    }

    impl HeartbeatModel {
        /// An empty registry carrier with `registrants` publishers, of which `stalled` never
        /// compact.
        fn new(registrants: u32, stalled: BTreeSet<u32>) -> Result<Self> {
            let did = entry::Entry::gen_did(MODEL_TOPIC).map_err(Error::CoreError)?;
            Ok(Self {
                owner: entry::Entry::new(did, vec![], entry::EntryKind::Data),
                published: (0..registrants)
                    .map(|registrant| (registrant, BTreeSet::new()))
                    .collect(),
                snapshots: BTreeMap::new(),
                last_compactions: BTreeMap::new(),
                stalled,
                now_ms: MODEL_EPOCH_MS,
                tombstones_issued: 0,
                compactors: vec![],
            })
        }

        /// A data carrier of the model's topic holding `data`, the payload of one operation.
        fn operand(&self, data: Vec<Encoded>) -> entry::Entry {
            entry::Entry::new(self.owner.did, data, entry::EntryKind::Data)
        }

        /// Apply `op` from `actor` to the owner at the next operation instant.
        fn apply(&mut self, actor: u32, op: entry::EntryOperation) -> Result<()> {
            self.now_ms = self.now_ms.saturating_add(1);
            self.owner = self
                .owner
                .operate(self.now_ms, op, Did::from(actor))
                .map_err(Error::CoreError)?;
            Ok(())
        }

        /// Heartbeat `heartbeat` of `registrant`.
        fn heartbeat(&mut self, registrant: u32, heartbeat: usize) -> Result<()> {
            let current = BTreeSet::from([model_descriptor(registrant, heartbeat)]);
            let observation = RegistryObservation {
                entry: self.snapshots.insert(registrant, self.owner.clone()),
                local: Did::from(registrant),
                last_compaction: self.last_compactions.get(&registrant).copied(),
            };
            let plan = plan_compacting_registration_publish(
                self.published.entry(registrant).or_default(),
                &current,
                observation,
                |value| model_live_registrant(value) == Some(Did::from(registrant)),
                model_live_registrant,
            )?;
            for value in current.iter() {
                let op = entry::EntryOperation::Extend(self.operand(vec![value.clone()]));
                self.apply(registrant, op)?;
            }
            for stale in plan.removals.iter() {
                let op = entry::EntryOperation::Tombstone(self.operand(vec![stale.clone()]));
                self.apply(registrant, op)?;
                self.tombstones_issued = self.tombstones_issued.saturating_add(1);
            }
            if let Some(mark) = plan
                .compaction
                .filter(|_| !self.stalled.contains(&registrant))
            {
                let op = entry::EntryOperation::CompactData(self.operand(plan.removals));
                self.apply(registrant, op)?;
                self.last_compactions.insert(registrant, mark);
                self.compactors.push(registrant);
            }
            finish_registration_publish(self.published.entry(registrant).or_default(), current);
            Ok(())
        }

        /// One heartbeat of every registrant, in registrant order.
        fn round(&mut self, heartbeat: usize) -> Result<()> {
            let registrants = self.published.keys().copied().collect::<Vec<_>>();
            registrants
                .into_iter()
                .try_for_each(|registrant| self.heartbeat(registrant, heartbeat))
        }

        /// Run rounds `rounds`, checking `invariant` after each.
        fn run(&mut self, rounds: std::ops::Range<usize>, invariant: impl Fn(&Self)) -> Result<()> {
            rounds.into_iter().try_for_each(|heartbeat| {
                self.round(heartbeat)?;
                invariant(self);
                Ok(())
            })
        }

        /// The number of registrants.
        fn registrants(&self) -> usize {
            self.published.len()
        }

        /// The owner's tombstone count.
        fn tombstones(&self) -> usize {
            self.owner.crdt.tombstones.len()
        }

        /// The model's tombstone bound `T·(f + 1) + λ·Δ`, with `Δ` one tombstone per registrant
        /// per interval and `f` the stalled registrants.
        fn tombstone_bound(&self) -> usize {
            compaction_threshold(self.stalled.len()) + MODEL_READ_LAG_INTERVALS * self.registrants()
        }

        /// The registrant of rank 0 for a carrier with `register`, among `registrants`.
        fn designated(&self, register: Option<entry::EntryVersion>) -> Result<Option<u32>> {
            let anchor = CompactionMark { register }.anchor(self.owner.did)?;
            let registrants = self.published.keys().copied().collect::<Vec<_>>();
            Ok(registrants.iter().copied().find(|registrant| {
                compactor_rank(
                    anchor,
                    Did::from(*registrant),
                    registrants.iter().copied().map(Did::from),
                ) == Some(0)
            }))
        }
    }

    #[test]
    fn test_registration_heartbeats_compact_once_per_crossing_under_lagged_reads() -> Result<()> {
        let mut model = HeartbeatModel::new(8, BTreeSet::new())?;
        let rounds = REGISTRY_COMPACTION_TOMBSTONE_STEP / model.registrants();
        // Below the crossing: every heartbeat replaces a descriptor, none compacts.
        model.run(0..rounds, |model| {
            assert!(model.tombstones() < REGISTRY_COMPACTION_TOMBSTONE_STEP);
            assert!(model.compactors.is_empty());
        })?;
        // Across it: exactly the designated registrant compacts, once, although every
        // registrant's lagged read shows the crossing and the compactor's own next read still
        // carries the pre-floor register.
        let designated = model.designated(None)?;
        model.run(
            rounds..rounds + 2 * MODEL_READ_LAG_INTERVALS + 2,
            |_model| (),
        )?;
        assert_eq!(model.compactors, designated.into_iter().collect::<Vec<_>>());
        assert!(model.tombstones() < REGISTRY_COMPACTION_TOMBSTONE_STEP);
        Ok(())
    }

    #[test]
    fn test_registration_heartbeats_keep_the_registry_carrier_bounded() -> Result<()> {
        let mut model = HeartbeatModel::new(5, BTreeSet::new())?;
        let registrants = model.registrants();
        let rounds = 3 * REGISTRY_COMPACTION_TOMBSTONE_STEP / registrants;
        model.run(0..rounds, |model| {
            assert!(model.tombstones() < model.tombstone_bound());
            assert_eq!(model.owner.data.len(), registrants);
            assert_eq!(model.owner.crdt.dots.len(), registrants);
        })?;
        // One compaction per step of tombstones, and the bound was kept by compacting.
        let compactions = model.compactors.len();
        assert!(compactions * REGISTRY_COMPACTION_TOMBSTONE_STEP <= model.tombstones_issued);
        assert!(compactions >= 2);
        Ok(())
    }

    #[test]
    fn test_registration_compaction_ladder_overtakes_a_stalled_designee() -> Result<()> {
        let probe = HeartbeatModel::new(4, BTreeSet::new())?;
        let stalled = probe.designated(None)?.into_iter().collect::<BTreeSet<_>>();
        assert_eq!(stalled.len(), 1);
        let mut model = HeartbeatModel::new(4, stalled.clone())?;
        let rounds = compaction_threshold(1) / model.registrants() + 2 * MODEL_READ_LAG_INTERVALS;
        model.run(0..rounds, |model| {
            assert!(model.tombstones() < model.tombstone_bound());
        })?;
        // Rank 1 took over at 2T; the stalled designee never compacted.
        assert_eq!(model.compactors.len(), 1);
        assert!(model
            .compactors
            .iter()
            .all(|compactor| !stalled.contains(compactor)));
        Ok(())
    }

    /// A model warmed up for `warmup` rounds, and a replica of its owner holding one add the
    /// owner never receives.
    fn model_with_replica_only_add(
        warmup: usize,
    ) -> Result<(HeartbeatModel, entry::Entry, Encoded)> {
        let mut model = HeartbeatModel::new(3, BTreeSet::new())?;
        model.run(0..warmup, |_model| ())?;
        let late_registrant = 99;
        let late = model_descriptor(late_registrant, 0);
        let replica = model
            .owner
            .operate(
                model.now_ms,
                entry::EntryOperation::Extend(model.operand(vec![late.clone()])),
                Did::from(late_registrant),
            )
            .map_err(Error::CoreError)?;
        assert!(!model.owner.data.contains(&late));
        Ok((model, replica, late))
    }

    #[test]
    fn test_registration_heartbeats_below_the_threshold_preserve_a_replica_only_add() -> Result<()>
    {
        let (mut model, replica, late) = model_with_replica_only_add(4)?;
        // Every heartbeat below replaces a descriptor, which used to stamp a reset floor.
        model.run(4..16, |_model| ())?;
        let synced_at_owner = model
            .owner
            .join(replica.clone())
            .map_err(Error::CoreError)?;
        let synced_at_replica = replica
            .join(model.owner.clone())
            .map_err(Error::CoreError)?;
        assert!(synced_at_owner.data.contains(&late));
        assert_eq!(synced_at_owner, synced_at_replica);
        assert!(model.compactors.is_empty());
        Ok(())
    }

    #[test]
    fn test_registration_compaction_past_the_threshold_still_erases_a_replica_only_add(
    ) -> Result<()> {
        // The residual #867 hazard this publisher narrows but cannot remove: the floor of a
        // crossing erases an add the owner never received, from every carrier that joins it.
        let (mut model, replica, late) = model_with_replica_only_add(4)?;
        let rounds = REGISTRY_COMPACTION_TOMBSTONE_STEP / model.registrants();
        model.run(4..rounds + 2 * MODEL_READ_LAG_INTERVALS, |_model| ())?;
        assert_eq!(model.compactors.len(), 1);
        let synced_at_replica = replica
            .join(model.owner.clone())
            .map_err(Error::CoreError)?;
        assert!(!synced_at_replica.data.contains(&late));
        Ok(())
    }

    fn encoded_subset(mask: u8) -> BTreeSet<Encoded> {
        ["a", "b", "c"]
            .into_iter()
            .enumerate()
            .filter(|(bit, _value)| mask & (1 << bit) != 0)
            .map(|(_bit, value)| encoded(value))
            .collect()
    }

    #[test]
    fn test_registration_publish_remembers_attempted_values_before_effects() {
        let old = encoded("old");
        let attempted = encoded("attempted");
        let current = BTreeSet::from([attempted.clone()]);
        let mut known = BTreeSet::from([old.clone()]);

        let stale = begin_registration_publish(&mut known, &current, vec![], |_| false);

        assert_eq!(stale, vec![old.clone()]);
        assert_eq!(known, BTreeSet::from([old, attempted]));
    }

    #[test]
    fn test_registration_publish_retry_tombstones_values_from_cancelled_attempts() {
        let old = encoded("old");
        let cancelled = encoded("cancelled");
        let replacement = encoded("replacement");
        let mut known = BTreeSet::from([old.clone()]);

        let cancelled_current = BTreeSet::from([cancelled.clone()]);
        let _ = begin_registration_publish(&mut known, &cancelled_current, vec![], |_| false);
        let replacement_current = BTreeSet::from([replacement.clone()]);
        let stale = begin_registration_publish(&mut known, &replacement_current, vec![], |_| false);

        assert_eq!(
            stale.into_iter().collect::<BTreeSet<_>>(),
            BTreeSet::from([old, cancelled])
        );
        assert!(known.contains(&replacement));
        finish_registration_publish(&mut known, replacement_current.clone());
        assert_eq!(known, replacement_current);
    }

    #[test]
    fn test_registration_publish_begin_finish_preserve_known_set_law() {
        for old_mask in 0..8 {
            for current_mask in 0..8 {
                for replacement_mask in 0..8 {
                    let old = encoded_subset(old_mask);
                    let current = encoded_subset(current_mask);
                    let replacement = encoded_subset(replacement_mask);
                    let mut known = old.clone();

                    let _ = begin_registration_publish(&mut known, &current, vec![], |_| false);
                    let attempted = old.union(&current).cloned().collect::<BTreeSet<_>>();
                    assert_eq!(known, attempted);

                    let stale =
                        begin_registration_publish(&mut known, &replacement, vec![], |_| false)
                            .into_iter()
                            .collect::<BTreeSet<_>>();
                    let expected_stale = attempted
                        .difference(&replacement)
                        .cloned()
                        .collect::<BTreeSet<_>>();
                    assert_eq!(stale, expected_stale);

                    finish_registration_publish(&mut known, replacement.clone());
                    assert_eq!(known, replacement);
                }
            }
        }
    }

    #[test]
    fn test_registration_publish_tombstones_matching_observed_values() {
        let current = BTreeSet::from([encoded("self-new")]);
        let observed_self_old = encoded("self-old");
        let observed_other = encoded("other");
        let mut known = BTreeSet::new();

        let stale = begin_registration_publish(
            &mut known,
            &current,
            vec![observed_self_old.clone(), observed_other],
            |observed| observed == &observed_self_old,
        );

        assert_eq!(stale, vec![observed_self_old]);
        assert_eq!(known, current);
    }

    #[test]
    fn test_registration_pruning_removes_replaced_or_unpreserved_observed_values() {
        let observed_self_old = encoded("self-old");
        let observed_live = encoded("other-live");
        let observed_invalid = encoded("invalid");

        let should_prune = |observed: &Encoded| {
            should_prune_observed_registry_value(
                observed,
                &|value| value == &observed_self_old,
                &|value| value == &observed_live,
            )
        };

        assert!(should_prune(&observed_self_old));
        assert!(!should_prune(&observed_live));
        assert!(should_prune(&observed_invalid));
    }

    #[test]
    fn test_registration_publish_tombstones_unpreserved_observed_values() {
        let current = BTreeSet::from([encoded("self-new")]);
        let observed_self_old = encoded("self-old");
        let observed_live = encoded("other-live");
        let observed_invalid = encoded("invalid");
        let mut known = BTreeSet::new();

        let stale = begin_registration_publish(
            &mut known,
            &current,
            vec![
                observed_self_old.clone(),
                observed_live,
                observed_invalid.clone(),
            ],
            |observed| observed == &observed_self_old || observed == &observed_invalid,
        );

        assert_eq!(
            stale.into_iter().collect::<BTreeSet<_>>(),
            BTreeSet::from([observed_invalid, observed_self_old])
        );
        assert_eq!(known, current);
    }
}
