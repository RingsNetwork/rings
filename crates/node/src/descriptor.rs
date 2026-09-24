//! Shared helpers for signed DHT descriptors.
//!
//! A descriptor is signed by the node's [`MessageSigner`], so its signature is bound to the
//! signer's overlay exactly like every other message, and verified under the *receiver's*
//! overlay. The body also carries `network_id` as a signed field; signing refuses a body whose
//! stated overlay differs from the signer's, so a verified descriptor's stated overlay is the
//! overlay it was signed for, and a descriptor issued in one overlay does not verify in another.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error;
use rings_core::error::Result;
use rings_core::message::DomainTag;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::MessageSigner;
use rings_core::message::MessageVerification;
use rings_core::message::SigningDomain;
use serde::de::DeserializeOwned;
use serde::Serialize;

/// The signed fields of one descriptor kind.
pub(crate) trait SignedDescriptorBody: Sized {
    type Descriptor;

    /// Message family of this descriptor kind.
    const DOMAIN_TAG: DomainTag;

    fn body_did(&self) -> Did;
    fn body_public_key(&self) -> &VerificationPublicKey;
    fn body_network_id(&self) -> u32;
    fn body_signing_data(&self) -> Result<Vec<u8>>;
    fn into_signed_descriptor(self, signature: MessageVerification) -> Self::Descriptor;
}

/// Sign `body` with the node's authority.
///
/// Pre: the body names the signer's account and public key, and states the signer's overlay.
pub(crate) fn sign_descriptor_body<B>(
    body: B,
    signer: MessageSigner<&DelegateeKey>,
    mismatch_message: &'static str,
) -> Result<B::Descriptor>
where
    B: SignedDescriptorBody,
{
    let did = body.body_did();
    if body.body_public_key().did() != did || signer.delegator_did() != did {
        return Err(Error::InvalidMessage(mismatch_message.to_string()));
    }
    if body.body_network_id() != signer.network_id() {
        return Err(Error::InvalidMessage(
            "descriptor states an overlay other than the signer's".to_string(),
        ));
    }

    let signature = signer.sign(B::DOMAIN_TAG, &body.body_signing_data()?)?;
    Ok(body.into_signed_descriptor(signature))
}

/// A published descriptor: its body plus the signature over it.
pub(crate) trait SignedDescriptor: Sized {
    /// The body this descriptor was signed from; fixes the message family.
    type Body: SignedDescriptorBody<Descriptor = Self>;

    fn descriptor_did(&self) -> Did;
    fn descriptor_public_key(&self) -> &VerificationPublicKey;
    /// The overlay the body states.
    fn descriptor_network_id(&self) -> u32;
    fn descriptor_signature(&self) -> &MessageVerification;
    fn descriptor_heartbeat_at_ms(&self) -> u128;
    fn descriptor_expires_at_ms(&self) -> u128;
    fn descriptor_signing_data(&self) -> Result<Vec<u8>>;

    /// Verify the signature under the receiver's overlay `network_id`, the DID/public-key
    /// binding of the signer, and that the body states that same overlay, so a verified
    /// descriptor's stated overlay is the overlay it was signed for.
    fn descriptor_verify_signature(&self, network_id: u32) -> bool {
        if self.descriptor_network_id() != network_id {
            return false;
        }
        let did = self.descriptor_did();
        let public_key = self.descriptor_public_key();
        let signature = self.descriptor_signature();
        if public_key.did() != did || signature.delegation.delegator_did() != did {
            return false;
        }

        let Ok(delegatee_public_key) = signature.delegation.delegator_verification_pubkey() else {
            return false;
        };
        if &delegatee_public_key != public_key {
            return false;
        }

        let Ok(data) = self.descriptor_signing_data() else {
            return false;
        };
        let domain =
            SigningDomain::new(<Self::Body as SignedDescriptorBody>::DOMAIN_TAG, network_id);
        signature.verify(domain, &data)
    }

    fn descriptor_is_expired_at(&self, now_ms: u128) -> bool {
        self.descriptor_expires_at_ms() < now_ms
    }

    fn descriptor_is_live_at(&self, now_ms: u128, network_id: u32) -> bool {
        self.descriptor_verify_signature(network_id) && !self.descriptor_is_expired_at(now_ms)
    }

    /// Return whether this descriptor supersedes `other` of the same key: the later heartbeat
    /// wins, and an equal heartbeat is decided by `(expiry, signed body, signature)`, so the
    /// order is total on distinct descriptors. A descriptor whose content fails to encode never
    /// supersedes, and is superseded by one that encodes.
    fn supersedes(&self, other: &Self) -> bool {
        let rank = |descriptor: &Self| {
            (
                descriptor.descriptor_heartbeat_at_ms(),
                descriptor.descriptor_expires_at_ms(),
            )
        };
        let content = |descriptor: &Self| {
            Some((
                descriptor.descriptor_signing_data().ok()?,
                rings_codec::serialize(descriptor.descriptor_signature()).ok()?,
            ))
        };
        rank(self)
            .cmp(&rank(other))
            .then_with(|| content(self).cmp(&content(other)))
            == std::cmp::Ordering::Greater
    }
}

/// Select the newest descriptor per DID that verifies under the receiver's overlay.
pub(crate) fn latest_valid_by_did<D>(
    descriptors: impl IntoIterator<Item = D>,
    now_ms: u128,
    network_id: u32,
    include_expired: bool,
) -> Vec<D>
where
    D: SignedDescriptor,
{
    latest_valid_by_key(
        descriptors,
        SignedDescriptor::descriptor_did,
        now_ms,
        network_id,
        include_expired,
    )
    .into_values()
    .collect()
}

/// Select the newest descriptor per `key` that verifies under the receiver's overlay, and live at
/// `now_ms` unless `include_expired`: the join `⊔` of the descriptors under
/// [`SignedDescriptor::supersedes`] within each key, a total order, so the selection does not
/// depend on the input order.
pub(crate) fn latest_valid_by_key<K, D>(
    descriptors: impl IntoIterator<Item = D>,
    key: impl Fn(&D) -> K,
    now_ms: u128,
    network_id: u32,
    include_expired: bool,
) -> BTreeMap<K, D>
where
    K: Ord,
    D: SignedDescriptor,
{
    let mut latest = BTreeMap::<K, D>::new();
    for descriptor in descriptors {
        if include_expired {
            if !descriptor.descriptor_verify_signature(network_id) {
                continue;
            }
        } else if !descriptor.descriptor_is_live_at(now_ms, network_id) {
            continue;
        }
        match latest.entry(key(&descriptor)) {
            Entry::Occupied(mut entry) => {
                if descriptor.supersedes(entry.get()) {
                    entry.insert(descriptor);
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(descriptor);
            }
        }
    }
    latest
}

pub(crate) fn encode_descriptor<T: Serialize>(descriptor: &T) -> Result<Encoded> {
    rings_codec::serialize(descriptor)
        .map_err(Error::CodecSerialize)?
        .encode()
}

pub(crate) fn decode_descriptor<T: DeserializeOwned>(encoded: &Encoded) -> Result<T> {
    let data: Vec<u8> = encoded.decode()?;
    rings_codec::deserialize(&data).map_err(Error::CodecDeserialize)
}
