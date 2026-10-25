//! The service descriptor: what a node announces through DNS relays, signed by its account.
//!
//! A node publishes its descriptor on the data topic of its own DID ([`service_topic`]), so a
//! relay finds an owner's descriptor with one fetch. Any node may write to any topic, so the
//! topic only locates: a relay takes the newest descriptor that verifies and is live, and is
//! signed for the DID the name holds (`latest_valid_by_did`). The descriptor states only ports,
//! never service names: the node serves port `p` as [`relay_service(p)`](super::relay_service),
//! so the relay's ingress can only ever ask the node for a service the node registered for its
//! name. Each port says whether its service reads a PROXY protocol header first.
//!
//! ```text
//!   σ(d) = (d, pk, network, ports : Port ⇀ ProxyProtocol, txt : SubName ⇀ [Value],
//!           freshness, lease end, sig)        heard at the dot of its element
//!   announcement(σ) = Announcement(ports, txt)       validated, or rejected
//! ```

use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::domain_tag;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error;
use rings_core::error::Result;
use rings_core::message::DomainTag;
use rings_core::message::MessageSigner;
use rings_core::message::MessageVerification;
use rings_dns_relay::Announcement;
use rings_dns_relay::AnnouncementError;
use rings_dns_relay::ProxyProtocol;
use rings_dns_relay::SubName;
use rings_dns_relay::TxtValue;
use serde::Deserialize;
use serde::Serialize;

use crate::descriptor::latest_valid_by_did;
use crate::descriptor::sign_descriptor_body;
use crate::descriptor::RegistryElement;
use crate::descriptor::SignedDescriptor;
use crate::descriptor::SignedDescriptorBody;

/// Message family of the service descriptor signature.
const SERVICE_DESCRIPTOR_DOMAIN_TAG: DomainTag = domain_tag!("rings-node:service-descriptor");

/// The data topic `did` publishes its service descriptor on.
///
/// Law: injective in `did`, and disjoint from every registry topic, none of which starts with
/// `dns_relay:`.
pub fn service_topic(did: Did) -> String {
    format!("dns_relay:{did}")
}

/// What a node serving `ports` and publishing `txt` announces, validated together.
pub(crate) fn announcement_of(
    ports: impl IntoIterator<Item = ServedPort>,
    txt: &[ServedTxt],
) -> std::result::Result<Announcement, AnnouncementError> {
    let ports = ports
        .into_iter()
        .map(|served| (served.port, served.proxy_protocol));
    let txt = txt
        .iter()
        .map(|served| {
            let values = served
                .values
                .iter()
                .map(|value| TxtValue::new(value.as_str()))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            SubName::new(served.name.as_str()).map(|name| (name, values))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Announcement::new(ports, txt)
}

/// One port a node serves at its relayed address.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct ServedPort {
    /// The port.
    pub port: u16,
    /// Whether the port's service reads a PROXY protocol header before the client's bytes.
    pub proxy_protocol: ProxyProtocol,
}

/// TXT records a node publishes at one name under its own.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct ServedTxt {
    /// The name under the node's name, such as `_acme-challenge`; empty for the node's own.
    pub name: String,
    /// The records' texts, one character-string each.
    pub values: Vec<String>,
}

/// Descriptor fields covered by the service-descriptor signature.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct ServiceDescriptorBody {
    /// DID of the node/account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Network identifier.
    pub network_id: u32,
    /// The ports the node serves at its address, ascending.
    pub ports: Vec<ServedPort>,
    /// The TXT records the node publishes under its name.
    pub txt: Vec<ServedTxt>,
    /// Freshness window `T` in milliseconds: a reader holds the descriptor live only while it
    /// heard it within `T` (see the lease law of `descriptor`).
    pub fresh_for_ms: u64,
    /// Lease end `ℓ` in milliseconds since Unix epoch.
    pub expires_at_ms: u128,
}

impl ServiceDescriptorBody {
    /// The bytes the signature covers.
    fn signing_data(&self) -> Result<Vec<u8>> {
        rings_codec::serialize(self).map_err(Error::CodecSerialize)
    }
}

impl SignedDescriptorBody for ServiceDescriptorBody {
    type Descriptor = ServiceDescriptor;

    const DOMAIN_TAG: DomainTag = SERVICE_DESCRIPTOR_DOMAIN_TAG;

    fn body_did(&self) -> Did {
        self.did
    }

    fn body_public_key(&self) -> &VerificationPublicKey {
        &self.public_key
    }

    fn body_network_id(&self) -> u32 {
        self.network_id
    }

    fn body_signing_data(&self) -> Result<Vec<u8>> {
        self.signing_data()
    }

    fn into_signed_descriptor(self, signature: MessageVerification) -> Self::Descriptor {
        ServiceDescriptor {
            body: self,
            signature,
            heartbeat_at_ms: 0,
        }
    }
}

/// Signed descriptor of the services a node announces through DNS relays.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct ServiceDescriptor {
    /// The signed fields.
    pub body: ServiceDescriptorBody,
    /// Signature covering `body`.
    pub signature: MessageVerification,
    /// When the reader last heard this descriptor, in milliseconds since Unix epoch: the dot of
    /// its element in the fetched service topic. Not signed and not encoded (see the lease law
    /// of `descriptor`); `0` until heard.
    #[serde(skip)]
    pub heartbeat_at_ms: u128,
}

impl ServiceDescriptor {
    /// Create and sign a descriptor.
    pub fn new_signed(
        body: ServiceDescriptorBody,
        signer: MessageSigner<&DelegateeKey>,
    ) -> Result<Self> {
        sign_descriptor_body(
            body,
            signer,
            "service descriptor DID/public key/session mismatch",
        )
    }

    /// What this descriptor announces: its ports and records, validated as one announcement.
    pub fn announcement(&self) -> std::result::Result<Announcement, AnnouncementError> {
        announcement_of(self.body.ports.iter().copied(), &self.body.txt)
    }

    /// The newest descriptor among `descriptors` that is signed for `did`, verifies under the
    /// overlay `network_id` and is live at `now_ms`.
    pub fn latest_valid_for(
        descriptors: impl IntoIterator<Item = Self>,
        did: Did,
        now_ms: u128,
        network_id: u32,
    ) -> Option<Self> {
        latest_valid_by_did(
            descriptors
                .into_iter()
                .filter(|descriptor| descriptor.body.did == did),
            now_ms,
            network_id,
            false,
        )
        .into_iter()
        .next()
    }
}

impl SignedDescriptor for ServiceDescriptor {
    type Body = ServiceDescriptorBody;

    fn descriptor_did(&self) -> Did {
        self.body.did
    }

    fn descriptor_public_key(&self) -> &VerificationPublicKey {
        &self.body.public_key
    }

    fn descriptor_network_id(&self) -> u32 {
        self.body.network_id
    }

    fn descriptor_signature(&self) -> &MessageVerification {
        &self.signature
    }

    fn descriptor_heartbeat_at_ms(&self) -> u128 {
        self.heartbeat_at_ms
    }

    fn descriptor_expires_at_ms(&self) -> u128 {
        self.body.expires_at_ms
    }

    fn descriptor_fresh_for_ms(&self) -> u64 {
        self.body.fresh_for_ms
    }

    fn descriptor_signing_data(&self) -> Result<Vec<u8>> {
        self.body.signing_data()
    }

    fn heard_at(self, heartbeat_at_ms: u128) -> Self {
        Self {
            heartbeat_at_ms,
            ..self
        }
    }
}

impl RegistryElement for ServiceDescriptor {}
