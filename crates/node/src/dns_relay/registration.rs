//! The heartbeat that keeps a node's service descriptor in the DHT.
//!
//! A descriptor is a registry element on the node's own [`service_topic`]: a signed lease whose
//! bytes each heartbeat re-appends ([`DhtRegistrationPublisher::publish_lease`], #918), so the
//! topic holds one live descriptor per node, written without a remove, and a reader stops
//! listing it within its freshness window once the node stops. A node that stops serving
//! publishes an offer with no port and no record, a new lease, which a relay reads as a
//! withdrawal.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use rings_core::ecc::VerificationPublicKey;
use rings_core::utils::get_epoch_ms;
use rings_dns_relay::AnnouncementError;
use rings_dns_relay::ProxyProtocol;

use super::descriptor::announcement_of;
use super::descriptor::service_topic;
use super::descriptor::ServedPort;
use super::descriptor::ServedTxt;
use super::descriptor::ServiceDescriptor;
use super::descriptor::ServiceDescriptorBody;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::rings_core::dht::Did;
use crate::registration::DhtRegistrationPublisher;
use crate::registration::LeaseTerms;
use crate::registration::RegistrationContext;
use crate::registration::RegistrationTask;

/// What a node offers through DNS relays: the ports it serves, each with how its service learns
/// a connection's origin, and its TXT records, validated as an announcement, with the timing of
/// the heartbeat that publishes them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceOffer {
    ports: BTreeMap<u16, ProxyProtocol>,
    txt: Vec<ServedTxt>,
    heartbeat_interval: Duration,
    ttl: Duration,
}

impl ServiceOffer {
    /// The offer of `ports` and `txt`, published every `heartbeat_interval` and live for `ttl`.
    ///
    /// Post: the descriptor this offer signs converts to a valid
    /// [`Announcement`](rings_dns_relay::Announcement); a relay never rejects what a node
    /// publishes.
    pub fn new(
        ports: BTreeMap<u16, ProxyProtocol>,
        txt: Vec<ServedTxt>,
        heartbeat_interval: Duration,
        ttl: Duration,
    ) -> std::result::Result<Self, AnnouncementError> {
        let offer = Self {
            ports,
            txt,
            heartbeat_interval,
            ttl,
        };
        announcement_of(offer.served_ports(), &offer.txt)?;
        Ok(offer)
    }

    /// The ports this offer serves, ascending.
    pub fn served_ports(&self) -> impl Iterator<Item = ServedPort> + '_ {
        self.ports.iter().map(|(port, proxy_protocol)| ServedPort {
            port: *port,
            proxy_protocol: *proxy_protocol,
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

    /// The unsigned descriptor body of this offer for `did`, with key `public_key` in overlay
    /// `network_id`, under the lease `terms`.
    fn body(
        &self,
        did: Did,
        public_key: VerificationPublicKey,
        network_id: u32,
        terms: LeaseTerms,
    ) -> ServiceDescriptorBody {
        ServiceDescriptorBody {
            did,
            public_key,
            network_id,
            ports: self.served_ports().collect(),
            txt: self.txt.clone(),
            fresh_for_ms: terms.fresh_for_ms,
            expires_at_ms: terms.expires_at_ms,
        }
    }
}

/// The registration task publishing a node's service descriptor.
#[derive(Clone, Debug)]
pub(crate) struct ServiceRegistration {
    offer: ServiceOffer,
    publisher: DhtRegistrationPublisher<[ServiceDescriptor; 1]>,
}

impl ServiceRegistration {
    /// The registration of `offer` for the node `did`.
    pub(crate) fn new(did: Did, offer: ServiceOffer) -> Self {
        Self {
            publisher: DhtRegistrationPublisher::new(
                service_topic(did),
                offer.heartbeat_interval,
                offer.ttl,
            ),
            offer,
        }
    }

    /// This node's signed descriptor under the lease `terms`.
    pub(crate) fn descriptor_at(
        &self,
        context: &RegistrationContext<'_>,
        terms: LeaseTerms,
    ) -> Result<ServiceDescriptor> {
        let body = self.offer.body(
            context.did(),
            context.delegator_verification_pubkey()?,
            context.network_id(),
            terms,
        );
        ServiceDescriptor::new_signed(body, context.message_signer()).map_err(Error::CoreError)
    }
}

#[cfg_attr(rings_browser, async_trait(?Send))]
#[cfg_attr(rings_native, async_trait)]
impl RegistrationTask for ServiceRegistration {
    fn name(&self) -> &'static str {
        "dns-relay-services"
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
