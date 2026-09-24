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
use rings_core::message::ChordStorageInterface;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::MessageSigner;
use rings_core::message::ScopedStorage;
use rings_core::utils::get_epoch_ms;
use rings_runtime::MaybeSendSync;

use crate::error::Error;
use crate::error::Result;
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

    /// The DHT operations of this registration: their rerouting waits end at its stop.
    fn storage(&self) -> ScopedStorage<'_> {
        self.processor.swarm.scoped_storage(self.stop.clone())
    }

    /// Append `data` to `topic`, stopping cooperatively with the registration.
    pub(crate) async fn append_data(&self, topic: &str, data: Encoded) -> Result<()> {
        self.storage()
            .storage_append_data(topic, data)
            .await
            .map_err(stoppable_storage_error)
    }

    /// Tombstone `data` in `topic`, stopping cooperatively with the registration.
    pub(crate) async fn tombstone_data(&self, topic: &str, data: Encoded) -> Result<()> {
        self.storage()
            .storage_tombstone_data(topic, data)
            .await
            .map_err(stoppable_storage_error)
    }

    /// Compact `removals` out of `topic`, stopping cooperatively with the registration.
    pub(crate) async fn compact_data(&self, topic: &str, removals: Vec<Encoded>) -> Result<()> {
        self.storage()
            .storage_compact_data(topic, removals)
            .await
            .map_err(stoppable_storage_error)
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
            context.append_data(&self.topic, value.clone()).await?;
        }
        for stale_value in stale_values {
            context.ensure_running()?;
            context
                .tombstone_data(&self.topic, stale_value.clone())
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

    /// Publish values, tombstone stale observed registry values, and compact at the owner.
    ///
    /// This never sends a replacement value set computed from an observed client
    /// snapshot. Compaction is requested with only the removable payloads, so the
    /// storage owner computes the final live set from its current local entry and
    /// preserves concurrent live writes.
    ///
    /// Each append, tombstone and compaction is rerouted as `Processor::storage_fetch`
    /// documents, and its rerouting wait ends at the registration's stop.
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
        let observed_values = observed_entry
            .as_ref()
            .map(|entry| entry.data.clone())
            .unwrap_or_default();
        let should_compact_metadata = observed_entry
            .as_ref()
            .is_some_and(registry_entry_has_compactable_metadata);
        let stale_values = {
            let mut published_values = self.published_values.lock().map_err(|_| Error::Lock)?;
            begin_registration_publish(
                &mut published_values,
                &current_values,
                observed_values,
                |observed| {
                    should_prune_observed_registry_value(
                        observed,
                        &replaces_observed_value,
                        &preserves_observed_value,
                    )
                },
            )
        };
        let should_compact = should_compact_metadata || !stale_values.is_empty();
        let removals = stale_values.clone();

        for value in &current_values {
            context.ensure_running()?;
            context.append_data(&self.topic, value.clone()).await?;
        }
        for stale_value in stale_values {
            context.ensure_running()?;
            context
                .tombstone_data(&self.topic, stale_value.clone())
                .await?;
            self.published_values
                .lock()
                .map_err(|_| Error::Lock)?
                .remove(&stale_value);
        }
        if should_compact {
            context.ensure_running()?;
            context.compact_data(&self.topic, removals).await?;
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

fn registry_entry_has_compactable_metadata(entry: &entry::Entry) -> bool {
    !entry.crdt.tombstones.is_empty()
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
    use std::collections::BTreeSet;

    use rings_core::message::Encoded;

    use super::*;

    fn encoded(value: &str) -> Encoded {
        value.into()
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
