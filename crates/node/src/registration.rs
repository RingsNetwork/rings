//! Node-layer DHT registration tasks.
//!
//! A registration task is a periodic node-side publisher. The task decides what
//! value to publish; [`DhtRegistrationPublisher`] owns the common DHT
//! touch/tombstone mechanics so new registries do not reimplement that state.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::lock::Mutex as AsyncMutex;
use rings_core::dht::Did;
use rings_core::ecc::VerificationPublicKey;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::session::SessionSk;
use rings_core::utils::get_epoch_ms;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::MaybeSend;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeDescriptorBody;
use crate::online::OnlineNodeType;
use crate::online::ONLINE_NODES_TOPIC;
#[cfg(feature = "snark")]
use crate::online::ONLINE_NODE_CAPABILITY_SNARK;
use crate::online::ONLINE_NODE_CAPABILITY_STORAGE;
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
    #[cfg(all(not(feature = "ffi"), feature = "browser"))]
    {
        OnlineNodeType::Browser
    }
    #[cfg(all(not(feature = "ffi"), not(feature = "browser")))]
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

#[cfg(not(feature = "browser"))]
pub(crate) async fn sleep_registration_interval(interval: Duration) -> Result<()> {
    // Native timers are infallible; the Result keeps the daemon shape shared
    // with the wasm arm, where browser timer setup can fail.
    futures_timer::Delay::new(interval).await;
    Ok(())
}

#[cfg(feature = "browser")]
pub(crate) async fn sleep_registration_interval(interval: Duration) -> Result<()> {
    let interval_ms = i32::try_from(interval.as_millis()).unwrap_or(i32::MAX);
    rings_core::utils::js_utils::window_sleep(interval_ms)
        .await
        .map_err(|error| Error::JsError(format!("{error:?}")))?;
    Ok(())
}

/// Capability passed to registration tasks.
///
/// The context exposes only the node facts and DHT publication operation that a
/// registry needs. The task does not own the processor.
pub struct RegistrationContext<'a> {
    processor: &'a Processor,
}

impl<'a> RegistrationContext<'a> {
    pub(crate) const fn new(processor: &'a Processor) -> Self {
        Self { processor }
    }

    /// Return the local node DID.
    pub fn did(&self) -> Did {
        self.processor.did()
    }

    /// Return the local network id.
    pub fn network_id(&self) -> u32 {
        self.processor.swarm.network_id()
    }

    /// Return storage redundancy for the local DHT protocol mode.
    pub fn storage_redundancy(&self) -> u16 {
        self.processor.swarm.storage_redundancy()
    }

    /// Return storage virtual-node positions for the local DHT protocol mode.
    pub fn dht_virtual_nodes(&self) -> u16 {
        self.processor.swarm.dht_virtual_nodes()
    }

    /// Return the account verification public key.
    pub fn account_verification_pubkey(&self) -> Result<VerificationPublicKey> {
        self.processor
            .swarm
            .account_verification_pubkey()
            .map_err(Error::CoreError)
    }

    /// Return the local session signing key.
    pub fn session_sk(&self) -> &SessionSk {
        self.processor.session_sk()
    }
}

/// Common publisher for DHT-backed registries.
#[derive(Clone, Debug)]
pub struct DhtRegistrationPublisher {
    topic: String,
    published_values: Arc<AsyncMutex<BTreeSet<Encoded>>>,
}

impl DhtRegistrationPublisher {
    /// Create a publisher for `topic`.
    pub fn new(topic: impl Into<String>) -> Self {
        Self {
            topic: topic.into(),
            published_values: Arc::new(AsyncMutex::new(BTreeSet::new())),
        }
    }

    /// Return the DHT topic used by this publisher.
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Publish `value`, tombstoning older values previously published by this publisher.
    pub async fn publish(&self, context: &RegistrationContext<'_>, value: Encoded) -> Result<()> {
        self.publish_many(context, std::iter::once(value)).await
    }

    /// Publish the current value set, tombstoning older values previously published by this publisher.
    ///
    /// Invariant: after a successful call, DHT data previously emitted by this publisher is exactly
    /// `values`. Preservation: every stale local value is tombstoned after every current value is
    /// touched under the same publisher serialization lock.
    pub async fn publish_many(
        &self,
        context: &RegistrationContext<'_>,
        values: impl IntoIterator<Item = Encoded>,
    ) -> Result<()> {
        let current_values = values.into_iter().collect::<BTreeSet<_>>();
        let mut published_values = self.published_values.lock().await;
        let stale_values = begin_registration_publish(&mut published_values, &current_values);

        for value in &current_values {
            context
                .processor
                .storage_touch_data(&self.topic, value.clone())
                .await?;
        }
        for stale_value in stale_values {
            context
                .processor
                .storage_tombstone_data(&self.topic, stale_value.clone())
                .await?;
            published_values.remove(&stale_value);
        }
        finish_registration_publish(&mut published_values, current_values);
        Ok(())
    }
}

fn begin_registration_publish(
    published_values: &mut BTreeSet<Encoded>,
    current_values: &BTreeSet<Encoded>,
) -> Vec<Encoded> {
    let stale_values = published_values
        .iter()
        .filter(|published| !current_values.contains(*published))
        .cloned()
        .collect::<Vec<_>>();
    // Invariant: every value whose touch may have reached storage is remembered
    // before the first await. If the publish future is later cancelled by an
    // attempt timeout, the next attempt can still tombstone the value.
    published_values.extend(current_values.iter().cloned());
    stale_values
}

fn finish_registration_publish(
    published_values: &mut BTreeSet<Encoded>,
    current_values: BTreeSet<Encoded>,
) {
    *published_values = current_values;
}

/// Periodic node-layer registration.
#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
pub trait RegistrationTask: MaybeSend {
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
    additional_capabilities: Vec<String>,
    publisher: DhtRegistrationPublisher,
}

impl OnlineNodeRegistration {
    /// Create an online-node registration task.
    pub fn new(
        heartbeat_interval: Duration,
        ttl: Duration,
        node_type: OnlineNodeType,
        endpoint_hint: Option<String>,
        additional_capabilities: Vec<String>,
    ) -> Self {
        Self {
            heartbeat_interval,
            ttl,
            node_type,
            started_at_ms: get_epoch_ms(),
            endpoint_hint,
            additional_capabilities,
            publisher: DhtRegistrationPublisher::new(ONLINE_NODES_TOPIC),
        }
    }

    /// Validate this registration's periodic schedule when it is enabled.
    pub fn validate_enabled_schedule(&self) -> Result<()> {
        validate_online_node_registration_timing(true, self.heartbeat_interval, self.ttl)
    }

    /// Return capability labels advertised by online-node descriptors.
    pub fn default_capabilities() -> Vec<String> {
        let capabilities = vec![ONLINE_NODE_CAPABILITY_STORAGE.to_string()];
        #[cfg(feature = "snark")]
        let capabilities = {
            let mut capabilities = capabilities;
            capabilities.push(ONLINE_NODE_CAPABILITY_SNARK.to_string());
            capabilities
        };
        capabilities
    }

    /// Return capability labels advertised by this registration.
    pub fn capabilities(&self) -> Vec<String> {
        let mut capabilities = Self::default_capabilities();
        for capability in &self.additional_capabilities {
            if !capabilities.iter().any(|known| known == capability) {
                capabilities.push(capability.clone());
            }
        }
        capabilities
    }

    /// Build this node's signed descriptor at `now_ms`.
    pub fn descriptor_at(
        &self,
        context: &RegistrationContext<'_>,
        now_ms: u128,
    ) -> Result<OnlineNodeDescriptor> {
        OnlineNodeDescriptor::new_signed(
            OnlineNodeDescriptorBody {
                did: context.did(),
                public_key: context.account_verification_pubkey()?,
                session_public_key: context.session_sk().session_public_key(),
                node_type: self.node_type.clone(),
                network_id: context.network_id(),
                storage_redundancy: context.storage_redundancy(),
                dht_virtual_nodes: context.dht_virtual_nodes(),
                capabilities: self.capabilities(),
                endpoint_hint: self.endpoint_hint.clone(),
                started_at_ms: self.started_at_ms,
                heartbeat_at_ms: now_ms,
                expires_at_ms: now_ms + self.ttl.as_millis(),
                version: crate::util::build_version(),
            },
            context.session_sk(),
        )
        .map_err(Error::CoreError)
    }

    /// Publish this node's signed online descriptor.
    pub async fn publish_descriptor(
        &self,
        context: &RegistrationContext<'_>,
    ) -> Result<OnlineNodeDescriptor> {
        let now_ms = get_epoch_ms();
        let descriptor = self.descriptor_at(context, now_ms)?;
        let encoded = descriptor.encode().map_err(Error::CoreError)?;
        self.publisher.publish(context, encoded).await?;
        Ok(descriptor)
    }

    /// Decode online-node descriptors from a DHT entry.
    pub fn descriptors_from_entry(
        entry: &rings_core::prelude::entry::Entry,
    ) -> Vec<OnlineNodeDescriptor> {
        entry
            .data
            .iter()
            .filter_map(|value| value.decode::<OnlineNodeDescriptor>().ok())
            .collect()
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
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
            .filter_map(|(bit, value)| (mask & (1 << bit) != 0).then(|| encoded(value)))
            .collect()
    }

    #[test]
    fn registration_publish_remembers_attempted_values_before_effects() {
        let old = encoded("old");
        let attempted = encoded("attempted");
        let current = BTreeSet::from([attempted.clone()]);
        let mut known = BTreeSet::from([old.clone()]);

        let stale = begin_registration_publish(&mut known, &current);

        assert_eq!(stale, vec![old.clone()]);
        assert_eq!(known, BTreeSet::from([old, attempted]));
    }

    #[test]
    fn registration_publish_retry_tombstones_values_from_cancelled_attempts() {
        let old = encoded("old");
        let cancelled = encoded("cancelled");
        let replacement = encoded("replacement");
        let mut known = BTreeSet::from([old.clone()]);

        let cancelled_current = BTreeSet::from([cancelled.clone()]);
        let _ = begin_registration_publish(&mut known, &cancelled_current);
        let replacement_current = BTreeSet::from([replacement.clone()]);
        let stale = begin_registration_publish(&mut known, &replacement_current);

        assert_eq!(
            stale.into_iter().collect::<BTreeSet<_>>(),
            BTreeSet::from([old, cancelled])
        );
        assert!(known.contains(&replacement));
        finish_registration_publish(&mut known, replacement_current.clone());
        assert_eq!(known, replacement_current);
    }

    #[test]
    fn registration_publish_begin_finish_preserve_known_set_law() {
        for old_mask in 0..8 {
            for current_mask in 0..8 {
                for replacement_mask in 0..8 {
                    let old = encoded_subset(old_mask);
                    let current = encoded_subset(current_mask);
                    let replacement = encoded_subset(replacement_mask);
                    let mut known = old.clone();

                    let _ = begin_registration_publish(&mut known, &current);
                    let attempted = old.union(&current).cloned().collect::<BTreeSet<_>>();
                    assert_eq!(known, attempted);

                    let stale = begin_registration_publish(&mut known, &replacement)
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
}
