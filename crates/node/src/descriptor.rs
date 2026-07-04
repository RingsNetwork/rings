//! Shared helpers for signed DHT descriptors.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use rings_core::dht::Did;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error;
use rings_core::error::Result;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::MessageVerification;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub(crate) trait SignedDescriptor: Sized {
    fn descriptor_did(&self) -> Did;
    fn descriptor_public_key(&self) -> &VerificationPublicKey;
    fn descriptor_signature(&self) -> &MessageVerification;
    fn descriptor_heartbeat_at_ms(&self) -> u128;
    fn descriptor_expires_at_ms(&self) -> u128;
    fn descriptor_signing_data(&self) -> Result<Vec<u8>>;

    fn descriptor_verify_signature(&self) -> bool {
        let did = self.descriptor_did();
        let public_key = self.descriptor_public_key();
        let signature = self.descriptor_signature();
        if public_key.did() != did || signature.session.account_did() != did {
            return false;
        }

        let Ok(session_public_key) = signature.session.account_verification_pubkey() else {
            return false;
        };
        if &session_public_key != public_key {
            return false;
        }

        let Ok(data) = self.descriptor_signing_data() else {
            return false;
        };
        signature.verify(&data)
    }

    fn descriptor_is_expired_at(&self, now_ms: u128) -> bool {
        self.descriptor_expires_at_ms() < now_ms
    }

    fn descriptor_is_live_at(&self, now_ms: u128) -> bool {
        self.descriptor_verify_signature() && !self.descriptor_is_expired_at(now_ms)
    }
}

pub(crate) fn latest_valid_by_did<D>(
    descriptors: impl IntoIterator<Item = D>,
    now_ms: u128,
    include_expired: bool,
) -> Vec<D>
where
    D: SignedDescriptor,
{
    let mut latest = BTreeMap::<Did, D>::new();
    for descriptor in descriptors {
        if include_expired {
            if !descriptor.descriptor_verify_signature() {
                continue;
            }
        } else if !descriptor.descriptor_is_live_at(now_ms) {
            continue;
        }
        match latest.entry(descriptor.descriptor_did()) {
            Entry::Occupied(mut entry) => {
                if descriptor.descriptor_heartbeat_at_ms()
                    > entry.get().descriptor_heartbeat_at_ms()
                {
                    entry.insert(descriptor);
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(descriptor);
            }
        }
    }
    latest.into_values().collect()
}

pub(crate) fn encode_descriptor<T: Serialize>(descriptor: &T) -> Result<Encoded> {
    bincode::serialize(descriptor)
        .map_err(Error::BincodeSerialize)?
        .encode()
}

pub(crate) fn decode_descriptor<T: DeserializeOwned>(encoded: &Encoded) -> Result<T> {
    let data: Vec<u8> = encoded.decode()?;
    bincode::deserialize(&data).map_err(Error::BincodeDeserialize)
}
