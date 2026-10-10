//! Node-layer DHT registration tasks.
//!
//! A registration task is a built-in periodic node-side publisher. The shared publisher owns
//! DHT touch/tombstone mechanics for the online-node and onion-exit registries.
//!
//! Boundedness: a registry carrier is a data topic, so each descriptor expires individually at
//! its dot's issue time plus the data element horizon `H = EntryKind::Data.max_lifetime_ms()`
//! unless it is written again, and the heartbeat is what writes it again. Each heartbeat adds
//! one descriptor and removes the one it replaces, and a remove retires `H + σ` after the dot
//! it covers, so a registry holds at most one live descriptor per registrant and service plus
//! the removes of the last `H + σ`: about `(H + σ) / heartbeat` per registrant, 200 removes of
//! 125 encoded bytes each at the default 30 s heartbeat. No publisher
//! compacts; a reset floor stamped by one owner would erase every concurrent descriptor that
//! owner never received (#867).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::lock::Mutex as AsyncMutex;
use rings_core::consts::TS_OFFSET_TOLERANCE_MS;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::entry;
use rings_core::dht::Did;
use rings_core::ecc::VerificationPublicKey;
use rings_core::lifecycle::StopToken;
use rings_core::message::MessageSigner;
use rings_core::utils::get_epoch_ms;
use rings_runtime::MaybeSendSync;

use crate::descriptor::prunes_registry_element;
use crate::descriptor::RegistryElement;
use crate::error::Error;
use crate::error::Result;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeDescriptorBody;
use crate::online::OnlineNodeType;
use crate::online::ONLINE_NODES_TOPIC;
use crate::processor::dht_lookup_poll_budget;
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

/// The heartbeat interval below which a registry descriptor stays stored between heartbeats.
///
/// A descriptor write requests the data default lifetime `L` (which bounds the registry of a
/// sole registrant, and is below the element horizon), stamped on the publisher's clock; an
/// owner whose clock runs up to `σ = TS_OFFSET_TOLERANCE_MS` ahead retires it at `L − σ` of the
/// publisher's time. Each heartbeat first fetches the registry, polling for up to the fetch-poll
/// budget `P`, before it appends, so the interval is below `L − σ − P`; the append's own
/// network latency must fit in what the interval leaves of it. The bound holds between
/// successful heartbeats: one that fails leaves a gap of two intervals, which a sole
/// registrant's descriptor outlives only if `2I < L − σ − P`.
pub(crate) fn registry_refresh_bound() -> Duration {
    let lifetime = Duration::from_millis(entry::EntryKind::Data.default_lifetime_ms());
    let skew = Duration::from_millis(u64::try_from(TS_OFFSET_TOLERANCE_MS).unwrap_or(u64::MAX));
    lifetime
        .saturating_sub(skew)
        .saturating_sub(dht_lookup_poll_budget())
}

/// Validate a registry heartbeat interval against [`registry_refresh_bound`]; `setting` names
/// the configuration key in the error.
pub(crate) fn validate_registry_heartbeat(
    setting: &str,
    heartbeat_interval: Duration,
) -> Result<()> {
    let bound = registry_refresh_bound();
    if heartbeat_interval >= bound {
        return Err(Error::InvalidConfig(format!(
            "{setting} ({heartbeat_interval:?}) must be less than {bound:?}, the lifetime of \
             a registry write less the clock-skew tolerance and the fetch-poll budget"
        )));
    }
    Ok(())
}

/// Validate online-node registration scheduling.
pub(crate) fn validate_online_node_registration_timing(
    advertise_presence: bool,
    heartbeat_interval: Duration,
    ttl: Duration,
) -> Result<()> {
    if !advertise_presence {
        return Ok(());
    }
    if heartbeat_interval >= ttl {
        return Err(Error::InvalidConfig(format!(
            "online_node_heartbeat_interval ({heartbeat_interval:?}) must be less than online_node_ttl ({ttl:?}) when advertise_presence is enabled"
        )));
    }
    validate_registry_heartbeat("online_node_heartbeat_interval", heartbeat_interval)
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
    published_values: Arc<Mutex<BTreeSet<Bytes>>>,
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

    /// Publish the current value set, tombstoning every observed value `prunes_observed_value`
    /// selects: older values with the same registry key, and, for a registry that prunes them,
    /// values that no longer verify or have expired.
    ///
    /// Invariant: registry topics are keyed presence sets, not append-only heartbeat logs.
    /// Preservation: every observed value replaced by the current publish is tombstoned after the
    /// replacement value has been touched, while unrelated publisher keys stay joinable.
    pub(crate) async fn publish_replacing(
        &self,
        context: &RegistrationContext<'_>,
        values: impl IntoIterator<Item = Bytes>,
        prunes_observed_value: impl Fn(&Bytes) -> bool,
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
                prunes_observed_value,
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

    async fn observed_registry_values(
        &self,
        context: &RegistrationContext<'_>,
    ) -> Result<Vec<Bytes>> {
        let entry_key = entry::Entry::gen_did(&self.topic)?;
        Ok(context
            .fetch_storage_entry(entry_key)
            .await?
            .map(|entry| entry.data)
            .unwrap_or_default())
    }
}

fn begin_registration_publish(
    published_values: &mut BTreeSet<Bytes>,
    current_values: &BTreeSet<Bytes>,
    observed_values: Vec<Bytes>,
    replaces_observed_value: impl Fn(&Bytes) -> bool,
) -> Vec<Bytes> {
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
    published_values: &mut BTreeSet<Bytes>,
    current_values: BTreeSet<Bytes>,
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
        let element = descriptor.to_element().map_err(Error::CoreError)?;
        let prunes_observed_value = |observed: &Bytes| {
            prunes_registry_element::<OnlineNodeDescriptor>(
                observed,
                context.did(),
                now_ms,
                context.network_id(),
            )
        };
        self.publisher
            .publish_replacing(context, std::iter::once(element), prunes_observed_value)
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
            .filter_map(|value| OnlineNodeDescriptor::from_element(value).ok())
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

    use super::*;

    /// The registry element holding the bytes of `value`.
    fn element(value: &'static str) -> Bytes {
        Bytes::from_static(value.as_bytes())
    }

    /// The subset of the elements `a`, `b`, `c` that `mask` selects bit by bit.
    fn element_subset(mask: u8) -> BTreeSet<Bytes> {
        ["a", "b", "c"]
            .into_iter()
            .enumerate()
            .filter(|(bit, _value)| mask & (1 << bit) != 0)
            .map(|(_bit, value)| element(value))
            .collect()
    }

    #[test]
    fn test_registration_publish_remembers_attempted_values_before_effects() {
        let old = element("old");
        let attempted = element("attempted");
        let current = BTreeSet::from([attempted.clone()]);
        let mut known = BTreeSet::from([old.clone()]);

        let stale = begin_registration_publish(&mut known, &current, vec![], |_| false);

        assert_eq!(stale, vec![old.clone()]);
        assert_eq!(known, BTreeSet::from([old, attempted]));
    }

    #[test]
    fn test_registration_publish_retry_tombstones_values_from_cancelled_attempts() {
        let old = element("old");
        let cancelled = element("cancelled");
        let replacement = element("replacement");
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
                    let old = element_subset(old_mask);
                    let current = element_subset(current_mask);
                    let replacement = element_subset(replacement_mask);
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
        let current = BTreeSet::from([element("self-new")]);
        let observed_self_old = element("self-old");
        let observed_other = element("other");
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
    fn test_registration_publish_tombstones_unpreserved_observed_values() {
        let current = BTreeSet::from([element("self-new")]);
        let observed_self_old = element("self-old");
        let observed_live = element("other-live");
        let observed_invalid = element("invalid");
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
