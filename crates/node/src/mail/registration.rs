//! The heartbeat that keeps a recipient's mail descriptor in the DHT.
//!
//! The descriptor is a registry element on the recipient's own
//! [`descriptor_topic`]: a signed lease whose bytes each heartbeat re-appends (the registry
//! publisher's heartbeat law, #918), so the topic holds
//! one live descriptor per recipient, written without a remove, and a gateway stops accepting
//! mail for it within its freshness window once the recipient stops.

use std::time::Duration;

use async_trait::async_trait;
use rings_core::utils::get_epoch_ms;

use super::descriptor::descriptor_topic;
use super::descriptor::MailDescriptor;
use super::descriptor::MailDescriptorBody;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::rings_core::dht::Did;
use crate::registration::validate_schedule;
use crate::registration::DhtRegistrationPublisher;
use crate::registration::LeaseTerms;
use crate::registration::RegistrationContext;
use crate::registration::RegistrationTask;
use crate::registration::TimingKeys;

/// The default interval between heartbeats of a mail descriptor.
pub const DEFAULT_MAIL_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// The default freshness window of a mail descriptor: three heartbeats.
pub const DEFAULT_MAIL_DESCRIPTOR_TTL: Duration = Duration::from_secs(90);

/// The schedule of a recipient's mail descriptor: how often it is published and how long each
/// publication is live.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MailOffer {
    heartbeat_interval: Duration,
    ttl: Duration,
}

impl MailOffer {
    /// The offer published every `heartbeat_interval` and live for `ttl`.
    ///
    /// Post: the schedule satisfies the registry's timing law (`validate_schedule`): the interval
    /// is below the window and below half the registry lease, and the window is at most the
    /// lease.
    pub fn new(heartbeat_interval: Duration, ttl: Duration) -> Result<Self> {
        let keys = TimingKeys {
            heartbeat_interval: "mail_heartbeat_interval",
            ttl: "mail_ttl",
            enabled_by: None,
        };
        validate_schedule(&keys, heartbeat_interval, ttl)?;
        Ok(Self {
            heartbeat_interval,
            ttl,
        })
    }

    /// How often the descriptor is published.
    pub fn heartbeat_interval(&self) -> Duration {
        self.heartbeat_interval
    }

    /// How long each published descriptor is live.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }
}

impl Default for MailOffer {
    /// [`DEFAULT_MAIL_HEARTBEAT_INTERVAL`] and [`DEFAULT_MAIL_DESCRIPTOR_TTL`], which satisfy
    /// the timing law.
    fn default() -> Self {
        Self {
            heartbeat_interval: DEFAULT_MAIL_HEARTBEAT_INTERVAL,
            ttl: DEFAULT_MAIL_DESCRIPTOR_TTL,
        }
    }
}

/// The registration task publishing a recipient's mail descriptor.
#[derive(Clone, Debug)]
pub(crate) struct MailRegistration {
    offer: MailOffer,
    publisher: DhtRegistrationPublisher<[MailDescriptor; 1]>,
}

impl MailRegistration {
    /// The registration of `offer` for the recipient `did`.
    pub(crate) fn new(did: Did, offer: MailOffer) -> Self {
        Self {
            publisher: DhtRegistrationPublisher::new(
                descriptor_topic(did),
                offer.heartbeat_interval,
                offer.ttl,
            ),
            offer,
        }
    }

    /// This recipient's signed descriptor under the lease `terms`, signed by its delegatee,
    /// whose key mail is sealed to.
    pub(crate) fn descriptor_at(
        &self,
        context: &RegistrationContext<'_>,
        terms: LeaseTerms,
    ) -> Result<MailDescriptor> {
        let body = MailDescriptorBody {
            did: context.did(),
            public_key: context.delegator_verification_pubkey()?,
            network_id: context.network_id(),
            fresh_for_ms: terms.fresh_for_ms,
            expires_at_ms: terms.expires_at_ms,
        };
        MailDescriptor::new_signed(body, context.message_signer()).map_err(Error::CoreError)
    }
}

#[cfg_attr(rings_browser, async_trait(?Send))]
#[cfg_attr(rings_native, async_trait)]
impl RegistrationTask for MailRegistration {
    fn name(&self) -> &'static str {
        "mail-descriptor"
    }

    fn interval(&self) -> Duration {
        self.offer.heartbeat_interval
    }

    async fn register_once(&self, context: &RegistrationContext<'_>) -> Result<()> {
        self.publisher
            .publish_lease(context, get_epoch_ms(), |terms| {
                self.descriptor_at(context, terms)
                    .map(|descriptor| [descriptor])
            })
            .await
            .map(drop)
    }
}
