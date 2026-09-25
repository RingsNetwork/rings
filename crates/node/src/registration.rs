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
}

impl DhtRegistrationPublisher {
    /// Create a publisher for `topic`.
    pub(crate) fn new(topic: impl Into<String>) -> Self {
        Self {
            topic: topic.into(),
            publish_gate: Arc::new(AsyncMutex::new(())),
            published_values: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// Publish the current value set, tombstoning older observed values with the same registry key.
    ///
    /// Invariant: registry topics are keyed presence sets, not append-only heartbeat logs.
    /// Preservation: every observed value replaced by the current publish is tombstoned after the
    /// replacement value has been touched, while unrelated publisher keys stay joinable.
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

    /// Publish values, tombstone stale observed registry values, and compact at the owner once
    /// the observed tombstone metadata crosses [`REGISTRY_COMPACTION_TOMBSTONE_THRESHOLD`].
    ///
    /// This never sends a replacement value set computed from an observed client
    /// snapshot. Compaction is requested with only the removable payloads, so the
    /// storage owner computes the final live set from its current local entry and
    /// preserves concurrent live writes it holds.
    ///
    /// A replaced descriptor is already removed by its per-dot tombstone, so a heartbeat does
    /// not compact merely because it replaced one: a compaction stamps a reset floor that also
    /// erases every concurrent add the owner has not received (#867), so it is issued only to
    /// bound tombstone metadata. The plan is [`plan_compacting_registration_publish`].
    pub(crate) async fn publish_many_replacing_and_compacting(
        &self,
        context: &RegistrationContext<'_>,
        values: impl IntoIterator<Item = Encoded>,
        replaces_observed_value: impl Fn(&Encoded) -> bool,
        preserves_observed_value: impl Fn(&Encoded) -> bool,
    ) -> Result<()> {
        let current_values = values.into_iter().collect::<BTreeSet<_>>();
        let _publish_turn = self.publish_gate.lock().await;
        context.ensure_running()?;
        let observed_entry = self.observed_registry_entry(context).await?;
        let plan = {
            let mut published_values = self.published_values.lock().map_err(|_| Error::Lock)?;
            plan_compacting_registration_publish(
                &mut published_values,
                &current_values,
                observed_entry.as_ref(),
                |observed| {
                    should_prune_observed_registry_value(
                        observed,
                        &replaces_observed_value,
                        &preserves_observed_value,
                    )
                },
            )
        };

        for value in &current_values {
            context.ensure_running()?;
            context
                .processor
                .storage_append_data(&self.topic, value.clone())
                .await?;
        }
        for stale_value in plan.tombstones.iter() {
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
        if plan.compacts {
            context.ensure_running()?;
            context
                .processor
                .storage_compact_data(&self.topic, plan.tombstones)
                .await?;
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

/// The observed tombstone count at which a registry publisher compacts its topic.
///
/// Derivation: a data carrier keeps at most `L = EntryKind::Data.max_data_len()` visible
/// elements, one add dot each, and the carrier law in `rings_core::consts` sizes a full
/// carrier for those `L` elements plus their dot metadata. Tombstones are the only part of a
/// data carrier with no cap of their own (`EntryKind::Data.max_tombstones()` is `None`); a
/// reset floor is what prunes them. Setting the threshold to `L` keeps the remove side of a
/// registry carrier at the scale of its add side:
///
/// ```text
///   |tombstones(owner)|  <  T + Δ         T = L,  Δ = tombstones issued in one heartbeat interval
///   |dots(owner)|        ≤  L             (materialization cap)
/// ```
///
/// because every live registrant observes the carrier once per heartbeat, so a crossing of `T`
/// is seen, and compacted, within one interval of it. A heartbeat below the threshold stamps
/// no floor, which is the point: a floor is the one operation that can erase a concurrent add
/// the owner has not received (#867).
pub(crate) const REGISTRY_COMPACTION_TOMBSTONE_THRESHOLD: usize =
    entry::EntryKind::Data.max_data_len();

/// Whether an observed registry carrier holds enough tombstone metadata to be compacted.
///
/// Predicate: `|tombstones(entry)| ≥ REGISTRY_COMPACTION_TOMBSTONE_THRESHOLD`. It is false for
/// an absent carrier and for every carrier below the threshold, whether or not the heartbeat
/// replaces a value.
fn registry_compaction_due(entry: &entry::Entry) -> bool {
    entry.crdt.tombstones.len() >= REGISTRY_COMPACTION_TOMBSTONE_THRESHOLD
}

/// The effects of one compacting registry heartbeat, computed purely from the publisher's
/// memory and one observation of the carrier.
///
/// The shell ([`DhtRegistrationPublisher::publish_many_replacing_and_compacting`]) executes it
/// in order: append the current values, tombstone [`Self::tombstones`], then compact with
/// [`Self::tombstones`] as the removals iff [`Self::compacts`].
#[derive(Clone, Debug, PartialEq, Eq)]
struct RegistrationPublishPlan {
    /// Previously published or observed values this heartbeat removes by per-dot tombstone.
    tombstones: Vec<Encoded>,
    /// Whether this heartbeat ends in a compaction: exactly `registry_compaction_due(observed)`.
    compacts: bool,
}

/// Plan one compacting registry heartbeat.
///
/// ```text
///   observed ──► begin_registration_publish ──► tombstones   (per-dot removals, no floor)
///      │
///      └──────► registry_compaction_due ─────► compacts      (floor only past the threshold)
/// ```
///
/// Law (no heartbeat floor): `compacts ⇔ registry_compaction_due(observed)`; in particular a
/// non-empty `tombstones` never implies a compaction on its own.
/// Effect on `published_values`: as [`begin_registration_publish`], every current value is
/// remembered before the first await.
fn plan_compacting_registration_publish(
    published_values: &mut BTreeSet<Encoded>,
    current_values: &BTreeSet<Encoded>,
    observed: Option<&entry::Entry>,
    prunes_observed_value: impl Fn(&Encoded) -> bool,
) -> RegistrationPublishPlan {
    let observed_values = observed.map_or_else(Vec::new, |entry| entry.data.clone());
    RegistrationPublishPlan {
        tombstones: begin_registration_publish(
            published_values,
            current_values,
            observed_values,
            prunes_observed_value,
        ),
        compacts: observed.is_some_and(registry_compaction_due),
    }
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
                        .is_ok_and(|descriptor| {
                            descriptor.verify_signature(context.network_id())
                                && !descriptor.is_expired_at(now_ms)
                        })
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

    /// Registrant `registrant`'s descriptor stand-in at heartbeat `heartbeat`.
    fn model_descriptor(registrant: u32, heartbeat: usize) -> Encoded {
        encoded(&format!("{registrant}:{heartbeat}"))
    }

    /// Whether `value` is a descriptor stand-in of `registrant`: the model's
    /// `replaces_observed_value`, which prunes only the registrant's own older descriptors.
    fn model_is_descriptor_of(registrant: u32, value: &Encoded) -> bool {
        value
            .value()
            .split_once(':')
            .is_some_and(|(prefix, _heartbeat)| prefix == registrant.to_string())
    }

    /// A registry carrier under compacting heartbeats, executed against a single owner.
    ///
    /// Each heartbeat observes the owner (a fresh read), plans with the production
    /// [`plan_compacting_registration_publish`], and applies the plan's effects in the order of
    /// [`DhtRegistrationPublisher::publish_many_replacing_and_compacting`]:
    ///
    /// ```text
    ///   observe owner ─► plan ─► Extend(current) ─► Tombstone(stale)* ─► CompactData? ─► finish
    /// ```
    struct HeartbeatModel {
        /// The owner's carrier.
        owner: entry::Entry,
        /// Each registrant's publisher memory.
        published: BTreeMap<u32, BTreeSet<Encoded>>,
        /// The next operation instant.
        now_ms: u128,
        /// Tombstone operations issued so far.
        tombstones_issued: usize,
        /// Compactions issued so far.
        compactions: usize,
    }

    impl HeartbeatModel {
        /// An empty registry carrier with `registrants` publishers.
        fn new(registrants: u32) -> Result<Self> {
            let did = entry::Entry::gen_did(MODEL_TOPIC).map_err(Error::CoreError)?;
            Ok(Self {
                owner: entry::Entry::new(did, vec![], entry::EntryKind::Data),
                published: (0..registrants)
                    .map(|registrant| (registrant, BTreeSet::new()))
                    .collect(),
                now_ms: MODEL_EPOCH_MS,
                tombstones_issued: 0,
                compactions: 0,
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

        /// Heartbeat `heartbeat` of `registrant`, returning whether it compacted.
        fn heartbeat(&mut self, registrant: u32, heartbeat: usize) -> Result<bool> {
            let current = BTreeSet::from([model_descriptor(registrant, heartbeat)]);
            let observed = Some(self.owner.clone());
            let plan = plan_compacting_registration_publish(
                self.published.entry(registrant).or_default(),
                &current,
                observed.as_ref(),
                |value| model_is_descriptor_of(registrant, value),
            );
            for value in current.iter() {
                let op = entry::EntryOperation::Extend(self.operand(vec![value.clone()]));
                self.apply(registrant, op)?;
            }
            for stale in plan.tombstones.iter() {
                let op = entry::EntryOperation::Tombstone(self.operand(vec![stale.clone()]));
                self.apply(registrant, op)?;
                self.tombstones_issued = self.tombstones_issued.saturating_add(1);
            }
            if plan.compacts {
                let op = entry::EntryOperation::CompactData(self.operand(plan.tombstones));
                self.apply(registrant, op)?;
                self.compactions = self.compactions.saturating_add(1);
            }
            finish_registration_publish(self.published.entry(registrant).or_default(), current);
            Ok(plan.compacts)
        }

        /// One heartbeat of every registrant, in registrant order.
        fn round(&mut self, heartbeat: usize) -> Result<()> {
            let registrants = self.published.keys().copied().collect::<Vec<_>>();
            registrants.into_iter().try_for_each(|registrant| {
                self.heartbeat(registrant, heartbeat).map(|_compacted| ())
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
    }

    #[test]
    fn test_registration_heartbeats_compact_only_on_a_threshold_crossing() -> Result<()> {
        let registrants = 4_u32;
        let mut model = HeartbeatModel::new(registrants)?;
        let rounds = REGISTRY_COMPACTION_TOMBSTONE_THRESHOLD / model.registrants() + 3;
        for heartbeat in 0..rounds {
            for registrant in 0..registrants {
                let due = model.tombstones() >= REGISTRY_COMPACTION_TOMBSTONE_THRESHOLD;
                let compacted = model.heartbeat(registrant, heartbeat)?;
                // Law: a heartbeat compacts iff its observation crossed the threshold; a
                // replaced descriptor alone never compacts.
                assert_eq!(compacted, due, "heartbeat {heartbeat} of {registrant}");
                if compacted {
                    // A compaction prunes every tombstone below its floor, so the next
                    // compaction needs a fresh crossing.
                    assert!(model.tombstones() < REGISTRY_COMPACTION_TOMBSTONE_THRESHOLD);
                }
            }
        }
        assert_eq!(model.compactions, 1);
        assert!(model.tombstones_issued >= REGISTRY_COMPACTION_TOMBSTONE_THRESHOLD);
        Ok(())
    }

    #[test]
    fn test_registration_heartbeats_keep_the_registry_carrier_bounded() -> Result<()> {
        let mut model = HeartbeatModel::new(5)?;
        let registrants = model.registrants();
        for heartbeat in 0..3 * REGISTRY_COMPACTION_TOMBSTONE_THRESHOLD / registrants {
            model.round(heartbeat)?;
            // Bound: at most one heartbeat interval (Δ = one tombstone per registrant) past T.
            assert!(model.tombstones() < REGISTRY_COMPACTION_TOMBSTONE_THRESHOLD + registrants);
            assert_eq!(model.owner.data.len(), registrants);
            assert_eq!(model.owner.crdt.dots.len(), registrants);
        }
        // At most one compaction per threshold's worth of tombstones, and the bound above was
        // reached by compacting, not by a lucky horizon.
        assert!(
            model.compactions * REGISTRY_COMPACTION_TOMBSTONE_THRESHOLD <= model.tombstones_issued
        );
        assert!(model.compactions >= 2);
        Ok(())
    }

    #[test]
    fn test_registration_heartbeats_preserve_a_concurrent_replica_only_add() -> Result<()> {
        let registrants = 3_u32;
        let late_registrant = registrants;
        let mut model = HeartbeatModel::new(registrants)?;
        (0..4).try_for_each(|heartbeat| model.round(heartbeat))?;
        // A replica holds an add the owner never receives.
        let late = model_descriptor(late_registrant, 0);
        let mut replica = model.owner.clone();
        replica = replica
            .operate(
                model.now_ms,
                entry::EntryOperation::Extend(model.operand(vec![late.clone()])),
                Did::from(late_registrant),
            )
            .map_err(Error::CoreError)?;
        assert!(!model.owner.data.contains(&late));

        // Every heartbeat below replaces a descriptor, which used to stamp a reset floor.
        (4..16).try_for_each(|heartbeat| model.round(heartbeat))?;
        assert_eq!(model.compactions, 0);

        // No floor was stamped, so the add survives sync in both directions.
        let synced_at_owner = model
            .owner
            .join(replica.clone())
            .map_err(Error::CoreError)?;
        let synced_at_replica = replica
            .join(model.owner.clone())
            .map_err(Error::CoreError)?;
        assert!(synced_at_owner.data.contains(&late));
        assert_eq!(synced_at_owner, synced_at_replica);
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
