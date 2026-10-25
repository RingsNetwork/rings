//! Shared helpers for signed DHT descriptors.
//!
//! A descriptor is signed by the node's [`MessageSigner`], so its signature is bound to the
//! signer's overlay exactly like every other message, and verified under the *receiver's*
//! overlay. The body also carries `network_id` as a signed field; signing refuses a body whose
//! stated overlay differs from the signer's, so a verified descriptor's stated overlay is the
//! overlay it was signed for, and a descriptor issued in one overlay does not verify in another.
//!
//! Lease law (#918; removal-stability paper, Laws 8 and 9): a descriptor is a signed lease. Its
//! body signs the lease end `expires_at_ms = ℓ` and the freshness window `fresh_for_ms = T`,
//! and nothing that changes between heartbeats, so its bytes are fixed for the lease and a
//! heartbeat re-appends them, which raises their dot without writing a remove. The time a
//! reader last heard the descriptor, `heartbeat_at_ms = τ*`, is the dot of its element in the
//! fetched carrier: it is not signed and not encoded, so a re-append or a forged dot can raise
//! it, but never past the signed `ℓ`. A reader at `t` holds the descriptor live iff it verifies,
//! its terms are admissible, `ℓ ≤ t + Λ + σ` and `T ≤ Λ` (`registry_lease`), and `t ≤ ℓ` and
//! `t ≤ τ* + T + σ`. The terms are the signer's choice, so the reader bounds them: a lease longer
//! than `Λ`, or a window longer than the lease, is never live, and a registrant that stops is
//! listed at most `Λ + σ` after its last write against any signer, writer or syncer.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::time::Duration;

use bytes::Bytes;
use rings_core::consts::DEFAULT_TTL_MS;
use rings_core::consts::TS_OFFSET_TOLERANCE_MS;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::entry::Entry as DhtEntry;
use rings_core::dht::Did;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error;
use rings_core::error::Result;
use rings_core::message::DomainTag;
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

/// The registry lease `Λ`: the data default lifetime `L`, so a replayed descriptor outlives its
/// registrant by at most `L` (removal-stability paper, Def. 7), and the bound a reader puts on
/// every signed lease and freshness window.
pub(crate) const fn registry_lease() -> Duration {
    Duration::from_millis(DEFAULT_TTL_MS)
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
    /// When a reader last heard this descriptor, `τ*`: unsigned (see the lease law).
    fn descriptor_heartbeat_at_ms(&self) -> u128;
    /// The signed lease end `ℓ`.
    fn descriptor_expires_at_ms(&self) -> u128;
    /// The signed freshness window `T`.
    fn descriptor_fresh_for_ms(&self) -> u64;
    fn descriptor_signing_data(&self) -> Result<Vec<u8>>;
    /// This descriptor as heard at `heartbeat_at_ms`, its element's dot: the one unsigned field.
    fn heard_at(self, heartbeat_at_ms: u128) -> Self;

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

    /// The last instant a reader holds this descriptor live: `min(ℓ, τ* + T + σ)`.
    fn descriptor_live_until_ms(&self) -> u128 {
        let heard_until = self
            .descriptor_heartbeat_at_ms()
            .saturating_add(u128::from(self.descriptor_fresh_for_ms()))
            .saturating_add(TS_OFFSET_TOLERANCE_MS);
        self.descriptor_expires_at_ms().min(heard_until)
    }

    /// Whether this descriptor is no longer live at `now_ms`: its lease ended, or it was not
    /// heard within its freshness window and the skew tolerance.
    fn descriptor_is_expired_at(&self, now_ms: u128) -> bool {
        self.descriptor_live_until_ms() < now_ms
    }

    /// Whether the signed lease terms are within the reader's bounds at `now_ms`: the lease
    /// ends at most `Λ + σ` from now and the freshness window is at most `Λ`. A longer lease or
    /// window is the signer's choice, not a registry's, so it is never live.
    fn descriptor_terms_admissible_at(&self, now_ms: u128) -> bool {
        let lease_ms = registry_lease().as_millis();
        self.descriptor_expires_at_ms()
            <= now_ms
                .saturating_add(lease_ms)
                .saturating_add(TS_OFFSET_TOLERANCE_MS)
            && u128::from(self.descriptor_fresh_for_ms()) <= lease_ms
    }

    /// Whether a reader at `now_ms` in overlay `network_id` lists this descriptor: it verifies,
    /// its terms are admissible, and it is not expired (the lease law).
    fn descriptor_is_live_at(&self, now_ms: u128, network_id: u32) -> bool {
        self.descriptor_verify_signature(network_id)
            && self.descriptor_terms_admissible_at(now_ms)
            && !self.descriptor_is_expired_at(now_ms)
    }

    /// Return whether this descriptor supersedes `other` of the same key: the later lease wins,
    /// then the later heartbeat, and a tie is decided by `(signed body, signature)`, so the order
    /// is total on distinct descriptors. The lease leads because it is signed: a replayed older
    /// lease can be heard later than the current one, but never outranks it. A descriptor whose
    /// content fails to encode never supersedes, and is superseded by one that encodes.
    fn supersedes(&self, other: &Self) -> bool {
        let rank = |descriptor: &Self| {
            (
                descriptor.descriptor_expires_at_ms(),
                descriptor.descriptor_heartbeat_at_ms(),
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

/// A descriptor as one element of its DHT registry: the element is the descriptor's codec
/// encoding, carried as is.
///
/// Law (round trip): `from_element(to_element(d)) = Ok(d)`.
pub(crate) trait RegistryElement: Serialize + DeserializeOwned {
    /// This descriptor as a registry element.
    fn to_element(&self) -> Result<Bytes> {
        rings_codec::serialize(self)
            .map(Bytes::from)
            .map_err(Error::CodecSerialize)
    }

    /// The descriptor the registry element `element` encodes.
    fn from_element(element: &[u8]) -> Result<Self> {
        rings_codec::deserialize(element).map_err(Error::CodecDeserialize)
    }
}

/// The registry elements of `entry`, each with the issue time of its dot, `τ*`: the time the
/// newest write of those bytes was stamped, which a heartbeat's re-append raises.
///
/// Law: a served carrier is normalized, so its dots align with its elements; an element without
/// a dot (none in a normalized carrier) is not heard and is skipped.
pub(crate) fn heard_elements(entry: &DhtEntry) -> impl Iterator<Item = (&Bytes, u128)> {
    entry.data.iter().zip(
        entry
            .crdt
            .dots
            .iter()
            .map(|dot| dot.version.logical_time_ms),
    )
}

/// Decode every element of `entry` as `D`, heard at its dot; elements that do not decode are
/// skipped.
pub(crate) fn heard_descriptors<D>(entry: &DhtEntry) -> Vec<D>
where D: SignedDescriptor + RegistryElement {
    heard_elements(entry)
        .filter_map(|(element, heard_at_ms)| {
            D::from_element(element)
                .ok()
                .map(|descriptor| descriptor.heard_at(heard_at_ms))
        })
        .collect()
}
