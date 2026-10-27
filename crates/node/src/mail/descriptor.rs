//! The mail descriptor `μ(d)`: a recipient's opt-in to mail, and the key mail is sealed to.
//!
//! ```text
//!   μ(d) = (d, pk, network, freshness, lease end, sig)          heard at the dot of its element
//!   sig  = the delegatee's signature, carrying the delegation that authorizes it
//!   ek   = recover(sig)                    the delegatee's secp256k1 public key, mail's key
//! ```
//!
//! The service descriptor of the DNS relay carries the account key, which a wallet account
//! (EIP-191, BIP-137) cannot decrypt with; mail is sealed to the delegatee's key instead.
//!
//! Law (key): `ek` is the key the signature recovers to (`MessageVerification::signer_key_at`),
//! so it is the key of the session that signed `μ(d)`, whose holder therefore published `μ(d)`
//! for `d`. The descriptor carries no copy of `ek` that could name another key.
//!
//! A recipient publishes `μ(d)` on its own [`descriptor_topic`], under the registry lease (#918,
//! the lease law of `descriptor`), so a gateway finds it with one fetch, and the newest live
//! descriptor wins (`latest_valid_by_did`).

use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::domain_tag;
use rings_core::ecc::PublicKey;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error;
use rings_core::error::Result;
use rings_core::message::DomainTag;
use rings_core::message::MessageSigner;
use rings_core::message::MessageVerification;
use rings_core::message::SigningDomain;
use serde::Deserialize;
use serde::Serialize;

use crate::descriptor::latest_valid_by_did;
use crate::descriptor::sign_descriptor_body;
use crate::descriptor::RegistryElement;
use crate::descriptor::SignedDescriptor;
use crate::descriptor::SignedDescriptorBody;

/// Message family of the mail descriptor signature.
const MAIL_DESCRIPTOR_DOMAIN_TAG: DomainTag = domain_tag!("rings-node:mail-descriptor");

/// The data topic `did` publishes its mail descriptor on: `"mail_descriptor:" ‖ d`.
///
/// Law: injective in `did`, and disjoint from [`mail_topic`](super::mail_topic) and from every
/// registry and DNS relay topic, none of which starts with `mail_descriptor:`.
pub fn descriptor_topic(did: Did) -> String {
    format!("mail_descriptor:{did}")
}

/// Where a gateway writes a recipient's mail, and the key it seals it to: what a live
/// [`MailDescriptor`] and its signature say.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MailRecipient {
    /// The recipient's DID, whose [`mail_topic`](super::mail_topic) the mail is written to.
    pub did: Did,
    /// The delegatee key the mail is sealed to.
    pub encryption_key: PublicKey<33>,
}

/// Descriptor fields covered by the mail-descriptor signature.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct MailDescriptorBody {
    /// DID of the recipient's account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Network identifier.
    pub network_id: u32,
    /// Freshness window `T` in milliseconds (see the lease law of `descriptor`).
    pub fresh_for_ms: u64,
    /// Lease end `ℓ` in milliseconds since Unix epoch.
    pub expires_at_ms: u128,
}

impl MailDescriptorBody {
    /// The bytes the signature covers.
    fn signing_data(&self) -> Result<Vec<u8>> {
        rings_codec::serialize(self).map_err(Error::CodecSerialize)
    }
}

impl SignedDescriptorBody for MailDescriptorBody {
    type Descriptor = MailDescriptor;

    const DOMAIN_TAG: DomainTag = MAIL_DESCRIPTOR_DOMAIN_TAG;

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
        MailDescriptor {
            body: self,
            signature,
            heartbeat_at_ms: 0,
        }
    }
}

/// Signed descriptor of a recipient that takes mail.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct MailDescriptor {
    /// The signed fields.
    pub body: MailDescriptorBody,
    /// Signature covering `body`, by the delegatee whose key mail is sealed to.
    pub signature: MessageVerification,
    /// When the reader last heard this descriptor, in milliseconds since Unix epoch: the dot of
    /// its element. Not signed and not encoded (see the lease law of `descriptor`); `0` until
    /// heard.
    #[serde(skip)]
    pub heartbeat_at_ms: u128,
}

impl MailDescriptor {
    /// Create and sign a descriptor.
    pub fn new_signed(
        body: MailDescriptorBody,
        signer: MessageSigner<&DelegateeKey>,
    ) -> Result<Self> {
        sign_descriptor_body(
            body,
            signer,
            "mail descriptor DID/public key/session mismatch",
        )
    }

    /// The key mail to this descriptor's recipient is sealed to, `ek`: the key its signature
    /// recovers to under the overlay `network_id` at `now_ms` (the key law), if it verifies.
    pub fn encryption_key_at(&self, network_id: u32, now_ms: u128) -> Option<PublicKey<33>> {
        let domain = SigningDomain::new(MAIL_DESCRIPTOR_DOMAIN_TAG, network_id);
        let data = self.body.signing_data().ok()?;
        self.signature.signer_key_at(domain, &data, now_ms)
    }

    /// Where mail to `did` goes and the key it is sealed to: the newest descriptor among
    /// `descriptors` that is signed for `did`, verifies under the overlay `network_id` and is
    /// live at `now_ms`, with its key.
    pub fn live_recipient(
        descriptors: impl IntoIterator<Item = Self>,
        did: Did,
        now_ms: u128,
        network_id: u32,
    ) -> Option<MailRecipient> {
        let descriptor = latest_valid_by_did(
            descriptors
                .into_iter()
                .filter(|descriptor| descriptor.body.did == did),
            now_ms,
            network_id,
            false,
        )
        .into_iter()
        .next()?;
        let encryption_key = descriptor.encryption_key_at(network_id, now_ms)?;
        Some(MailRecipient {
            did,
            encryption_key,
        })
    }
}

impl SignedDescriptor for MailDescriptor {
    type Body = MailDescriptorBody;

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

impl RegistryElement for MailDescriptor {}
