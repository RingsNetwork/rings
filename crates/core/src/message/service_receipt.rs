//! Canonical provisional service receipts and the Probe transcript.
//!
//! These receipt types are deliberately distinct from future ledger receipts.
//! Their signing domains and versioned canonical encodings cannot be reused by
//! a finalized-epoch protocol.

use std::num::NonZeroU64;

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

use super::Message;
use super::MessagePayload;
use super::MessageSigner;
use super::MessageVerification;
use super::MessageVerificationExt;
use super::SigningDomain;
use super::Transaction;
use crate::dht::Did;
use crate::domain_tag;
use crate::ecc::keccak256;
use crate::error::Result;

const CLAIM_WIRE_PREFIX: &[u8] = b"RINGS-PROVISIONAL-SERVICE-CLAIM\0";
const RECEIPT_WIRE_PREFIX: &[u8] = b"RINGS-PROVISIONAL-SERVICE-RECEIPT\0";
const PROVIDER_DOMAIN: super::DomainTag =
    domain_tag!("rings-core:service-receipt:provisional:provider");
const BENEFICIARY_DOMAIN: super::DomainTag =
    domain_tag!("rings-core:service-receipt:provisional:beneficiary");

/// Width of one provisional wall-clock epoch.
pub const PROVISIONAL_RECEIPT_EPOCH_SECS: u64 = 300;

/// Service kinds accepted by the v1 provisional receipt wire.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ServiceKind {
    /// One authenticated request/response probe.
    Probe,
}

/// Coarse wall-clock slot used only for live provisional collection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ProvisionalEpoch {
    /// Unix-time slice number.
    pub slot: u64,
}

impl ProvisionalEpoch {
    /// Derive the provisional slot from whole Unix seconds.
    pub const fn from_unix_seconds(seconds: u64) -> Self {
        Self {
            slot: seconds / PROVISIONAL_RECEIPT_EPOCH_SECS,
        }
    }

    /// Return whether this is the receiver's current or adjacent slot.
    pub const fn is_accepted_at(self, unix_seconds: u64) -> bool {
        let current = Self::from_unix_seconds(unix_seconds).slot;
        self.slot.abs_diff(current) <= 1
    }
}

/// Canonical v1 provisional service claim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProvisionalServiceClaim {
    /// Overlay in which the receipt signatures are valid.
    pub network_id: u32,
    /// Service evidence profile.
    pub service: ServiceKind,
    /// Account that supplied the service.
    pub provider_account: Did,
    /// Account that requested and acknowledged the service.
    pub beneficiary_account: Did,
    /// Provisional live-collection epoch.
    pub epoch: ProvisionalEpoch,
    /// Beneficiary-selected fresh nonce.
    pub nonce: [u8; 32],
    /// Service units; Probe requires exactly one.
    pub units: NonZeroU64,
    /// Digest of the exact signed request transaction.
    pub request_digest: [u8; 32],
    /// Digest of the exact signed completion transaction.
    pub completion_digest: [u8; 32],
}

impl ProvisionalServiceClaim {
    /// Construct the only v1 service profile, a one-unit probe.
    pub const fn probe(
        network_id: u32,
        provider_account: Did,
        beneficiary_account: Did,
        epoch: ProvisionalEpoch,
        nonce: [u8; 32],
        request_digest: [u8; 32],
        completion_digest: [u8; 32],
    ) -> Self {
        Self {
            network_id,
            service: ServiceKind::Probe,
            provider_account,
            beneficiary_account,
            epoch,
            nonce,
            units: NonZeroU64::MIN,
            request_digest,
            completion_digest,
        }
    }

    /// Validate the service rule and account-role relation.
    pub fn validate(&self) -> std::result::Result<(), ServiceReceiptError> {
        if self.provider_account == self.beneficiary_account {
            return Err(ServiceReceiptError::SameAccountRoles);
        }
        match self.service {
            ServiceKind::Probe if self.units == NonZeroU64::MIN => Ok(()),
            ServiceKind::Probe => Err(ServiceReceiptError::InvalidProbeUnits {
                units: self.units.get(),
            }),
        }
    }

    /// Serialize the claim under its explicit v1 marker.
    pub fn canonical_bytes(&self) -> std::result::Result<Vec<u8>, ServiceReceiptError> {
        self.validate()?;
        encode_prefixed(CLAIM_WIRE_PREFIX, self)
    }

    /// Parse only the canonical v1 claim representation.
    pub fn from_canonical_bytes(bytes: &[u8]) -> std::result::Result<Self, ServiceReceiptError> {
        let claim: Self = decode_prefixed(CLAIM_WIRE_PREFIX, bytes)?;
        claim.validate()?;
        Ok(claim)
    }

    /// Digest binding every field in the canonical claim.
    pub fn digest(&self) -> std::result::Result<ServiceReceiptDigest, ServiceReceiptError> {
        Ok(ServiceReceiptDigest(keccak256(&self.canonical_bytes()?)))
    }

    /// Sign the provider role domain with the delegated provider session.
    pub fn sign_provider(
        &self,
        signer: MessageSigner<&crate::session::SessionSk>,
    ) -> Result<MessageVerification> {
        self.require_signer(signer, self.provider_account)?;
        signer.sign(PROVIDER_DOMAIN, &self.canonical_bytes()?)
    }

    /// Sign the beneficiary role domain with the delegated beneficiary session.
    pub fn sign_beneficiary(
        &self,
        signer: MessageSigner<&crate::session::SessionSk>,
    ) -> Result<MessageVerification> {
        self.require_signer(signer, self.beneficiary_account)?;
        signer.sign(BENEFICIARY_DOMAIN, &self.canonical_bytes()?)
    }

    fn require_signer(
        &self,
        signer: MessageSigner<&crate::session::SessionSk>,
        expected: Did,
    ) -> std::result::Result<(), ServiceReceiptError> {
        if signer.network_id() != self.network_id {
            return Err(ServiceReceiptError::NetworkMismatch {
                expected: self.network_id,
                actual: signer.network_id(),
            });
        }
        let actual = signer.account_did();
        if actual != expected {
            return Err(ServiceReceiptError::SignerRoleMismatch { expected, actual });
        }
        Ok(())
    }

    fn verify_role_attestation(
        &self,
        attestation: &MessageVerification,
        expected_account: Did,
        domain: super::DomainTag,
        invalid: ServiceReceiptError,
    ) -> std::result::Result<(), ServiceReceiptError> {
        if attestation.session.account_did() != expected_account {
            return Err(ServiceReceiptError::SignerRoleMismatch {
                expected: expected_account,
                actual: attestation.session.account_did(),
            });
        }
        let claim = self.canonical_bytes()?;
        if !attestation.verify_at(
            SigningDomain::new(domain, self.network_id),
            &claim,
            attestation.ts_ms,
        ) {
            return Err(invalid);
        }
        Ok(())
    }

    fn verify_live_role_attestation_at(
        &self,
        attestation: &MessageVerification,
        expected_account: Did,
        domain: super::DomainTag,
        observed_at_ms: u128,
        not_live: ServiceReceiptError,
    ) -> std::result::Result<(), ServiceReceiptError> {
        if attestation.session.account_did() != expected_account {
            return Err(ServiceReceiptError::SignerRoleMismatch {
                expected: expected_account,
                actual: attestation.session.account_did(),
            });
        }
        let claim = self.canonical_bytes()?;
        if !attestation.verify_live_at(
            SigningDomain::new(domain, self.network_id),
            &claim,
            observed_at_ms,
        ) {
            return Err(not_live);
        }
        Ok(())
    }
}

/// Digest of one canonical provisional receipt or claim.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ServiceReceiptDigest([u8; 32]);

impl ServiceReceiptDigest {
    /// Construct a digest from canonical bytes.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the digest bytes.
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Complete two-role provisional receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProvisionalServiceReceipt {
    /// Canonical service claim.
    pub claim: ProvisionalServiceClaim,
    /// Provider-session signature under the provider role domain.
    pub provider_attestation: MessageVerification,
    /// Beneficiary-session signature under the beneficiary role domain.
    pub beneficiary_attestation: MessageVerification,
}

impl ProvisionalServiceReceipt {
    /// Assemble an acknowledged receipt without weakening either signature.
    pub fn new(
        claim: ProvisionalServiceClaim,
        provider_attestation: MessageVerification,
        beneficiary_attestation: MessageVerification,
    ) -> std::result::Result<Self, ServiceReceiptError> {
        let receipt = Self {
            claim,
            provider_attestation,
            beneficiary_attestation,
        };
        receipt.claim.validate()?;
        Ok(receipt)
    }

    /// Serialize the receipt under its explicit provisional v1 marker.
    pub fn canonical_bytes(&self) -> std::result::Result<Vec<u8>, ServiceReceiptError> {
        self.claim.validate()?;
        encode_prefixed(RECEIPT_WIRE_PREFIX, self)
    }

    /// Parse only the canonical provisional v1 representation.
    pub fn from_canonical_bytes(bytes: &[u8]) -> std::result::Result<Self, ServiceReceiptError> {
        let receipt: Self = decode_prefixed(RECEIPT_WIRE_PREFIX, bytes)?;
        receipt.claim.validate()?;
        Ok(receipt)
    }

    /// Digest used for deterministic ordering and duplicate detection.
    pub fn digest(&self) -> std::result::Result<ServiceReceiptDigest, ServiceReceiptError> {
        Ok(ServiceReceiptDigest(keccak256(&self.canonical_bytes()?)))
    }

    /// Verify portable cryptographic facts without claiming live observation.
    pub fn verify_crypto(
        &self,
        receiver_network_id: u32,
    ) -> std::result::Result<(), ServiceReceiptError> {
        self.claim.validate()?;
        if self.claim.network_id != receiver_network_id {
            return Err(ServiceReceiptError::NetworkMismatch {
                expected: receiver_network_id,
                actual: self.claim.network_id,
            });
        }
        self.claim.verify_role_attestation(
            &self.provider_attestation,
            self.claim.provider_account,
            PROVIDER_DOMAIN,
            ServiceReceiptError::InvalidProviderAttestation,
        )?;
        self.claim.verify_role_attestation(
            &self.beneficiary_attestation,
            self.claim.beneficiary_account,
            BENEFICIARY_DOMAIN,
            ServiceReceiptError::InvalidBeneficiaryAttestation,
        )
    }

    /// Verify cryptography, proof liveness, and current/adjacent epoch admission.
    pub fn verify_live_at(
        &self,
        receiver_network_id: u32,
        observed_at_ms: u128,
    ) -> std::result::Result<(), ServiceReceiptError> {
        self.verify_crypto(receiver_network_id)?;
        let seconds = u64::try_from(observed_at_ms / 1_000)
            .map_err(|_| ServiceReceiptError::ObservationTimeOverflow)?;
        self.claim.verify_live_role_attestation_at(
            &self.provider_attestation,
            self.claim.provider_account,
            PROVIDER_DOMAIN,
            observed_at_ms,
            ServiceReceiptError::ProviderAttestationNotLive,
        )?;
        self.claim.verify_live_role_attestation_at(
            &self.beneficiary_attestation,
            self.claim.beneficiary_account,
            BENEFICIARY_DOMAIN,
            observed_at_ms,
            ServiceReceiptError::BeneficiaryAttestationNotLive,
        )?;
        if !self.claim.epoch.is_accepted_at(seconds) {
            return Err(ServiceReceiptError::EpochOutsideTolerance {
                claim_slot: self.claim.epoch.slot,
                observed_slot: ProvisionalEpoch::from_unix_seconds(seconds).slot,
            });
        }
        Ok(())
    }
}

/// Beneficiary request initiating Probe.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProbeRequest {
    /// Provisional collection slot proposed by the beneficiary.
    pub epoch: ProvisionalEpoch,
    /// Fresh random request nonce.
    pub nonce: [u8; 32],
}

impl ProbeRequest {
    pub(crate) fn random_for_epoch(epoch: ProvisionalEpoch) -> Self {
        let mut nonce = [0_u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
        Self { epoch, nonce }
    }
}

#[cfg(test)]
pub(crate) fn test_probe_request(nonce: u8) -> ProbeRequest {
    let seconds = u64::try_from(crate::utils::get_epoch_ms() / 1_000).unwrap_or(0);
    ProbeRequest {
        epoch: ProvisionalEpoch::from_unix_seconds(seconds),
        nonce: [nonce; 32],
    }
}

/// Provider-signed completion transaction embedded in the offer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProbeCompletion {
    /// Digest of the exact request transaction this completes.
    pub request_digest: [u8; 32],
    /// Request nonce copied into the completion.
    pub nonce: [u8; 32],
}

/// Provider response carrying the two exact signed transcript transactions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProbeOffer {
    /// Beneficiary-signed request transaction.
    pub request: Transaction,
    /// Provider-signed completion transaction.
    pub completion: Transaction,
    /// Claim binding the transaction digests and account roles.
    pub claim: ProvisionalServiceClaim,
    /// Provider signature over the claim under its role domain.
    pub provider_attestation: MessageVerification,
}

impl ProbeOffer {
    /// Verify one self-consistent transcript with currently live embedded transactions.
    ///
    /// The caller must already have admitted the outer payload through the shared transport
    /// verification and replay boundary.
    pub fn verify_transcript(
        &self,
        outer: &MessagePayload,
        receiver_network_id: u32,
        beneficiary: Did,
    ) -> std::result::Result<ProbeRequest, ServiceReceiptError> {
        self.verify_transcript_at(
            outer,
            receiver_network_id,
            beneficiary,
            crate::utils::get_epoch_ms(),
        )
    }

    fn verify_transcript_at(
        &self,
        outer: &MessagePayload,
        receiver_network_id: u32,
        beneficiary: Did,
        observed_at_ms: u128,
    ) -> std::result::Result<ProbeRequest, ServiceReceiptError> {
        self.claim.validate()?;
        if self.claim.network_id != receiver_network_id {
            return Err(ServiceReceiptError::NetworkMismatch {
                expected: receiver_network_id,
                actual: self.claim.network_id,
            });
        }
        let provider = outer.transaction.origin();
        if outer.transaction.destination != beneficiary
            || self.claim.provider_account != provider
            || self.claim.beneficiary_account != beneficiary
        {
            return Err(ServiceReceiptError::TranscriptRoleMismatch);
        }
        if self.request.origin() != beneficiary
            || self.request.destination != provider
            || self.request.tx_id != outer.transaction.tx_id
            || !self.request.verify_at(receiver_network_id, observed_at_ms)
        {
            return Err(ServiceReceiptError::InvalidRequestTransaction);
        }
        let request_message: Message = self
            .request
            .data()
            .map_err(|_| ServiceReceiptError::InvalidRequestTransaction)?;
        let Message::ProbeRequest(request) = request_message else {
            return Err(ServiceReceiptError::InvalidRequestTransaction);
        };
        if request.epoch != self.claim.epoch || request.nonce != self.claim.nonce {
            return Err(ServiceReceiptError::TranscriptDigestMismatch);
        }
        let request_digest = self
            .request
            .digest()
            .map_err(|_| ServiceReceiptError::CanonicalEncoding)?
            .into_bytes();
        if request_digest != self.claim.request_digest {
            return Err(ServiceReceiptError::TranscriptDigestMismatch);
        }
        if self.completion.origin() != provider
            || self.completion.destination != beneficiary
            || self.completion.tx_id != outer.transaction.tx_id
            || !self
                .completion
                .verify_at(receiver_network_id, observed_at_ms)
        {
            return Err(ServiceReceiptError::InvalidCompletionTransaction);
        }
        let completion: ProbeCompletion = self
            .completion
            .data()
            .map_err(|_| ServiceReceiptError::InvalidCompletionTransaction)?;
        if completion.request_digest != request_digest || completion.nonce != request.nonce {
            return Err(ServiceReceiptError::TranscriptDigestMismatch);
        }
        let completion_digest = self
            .completion
            .digest()
            .map_err(|_| ServiceReceiptError::CanonicalEncoding)?
            .into_bytes();
        if completion_digest != self.claim.completion_digest {
            return Err(ServiceReceiptError::TranscriptDigestMismatch);
        }
        self.claim.verify_role_attestation(
            &self.provider_attestation,
            provider,
            PROVIDER_DOMAIN,
            ServiceReceiptError::InvalidProviderAttestation,
        )?;
        Ok(request)
    }

    /// Verify an offered transcript before the beneficiary signs it during live collection.
    pub fn verify_live_transcript_at(
        &self,
        outer: &MessagePayload,
        receiver_network_id: u32,
        beneficiary: Did,
        observed_at_ms: u128,
    ) -> std::result::Result<ProbeRequest, ServiceReceiptError> {
        let request =
            self.verify_transcript_at(outer, receiver_network_id, beneficiary, observed_at_ms)?;
        let seconds = u64::try_from(observed_at_ms / 1_000)
            .map_err(|_| ServiceReceiptError::ObservationTimeOverflow)?;
        self.claim.verify_live_role_attestation_at(
            &self.provider_attestation,
            self.claim.provider_account,
            PROVIDER_DOMAIN,
            observed_at_ms,
            ServiceReceiptError::ProviderAttestationNotLive,
        )?;
        if !self.claim.epoch.is_accepted_at(seconds) {
            return Err(ServiceReceiptError::EpochOutsideTolerance {
                claim_slot: self.claim.epoch.slot,
                observed_slot: ProvisionalEpoch::from_unix_seconds(seconds).slot,
            });
        }
        Ok(request)
    }
}

/// Beneficiary acknowledgement carrying the now-complete receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProbeAcknowledgement {
    /// Complete receipt admitted by the provider after verification.
    pub receipt: ProvisionalServiceReceipt,
}

/// Typed rejection reason for provisional receipt construction or verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum ServiceReceiptError {
    /// Canonical encoding or decoding failed.
    #[error("provisional receipt canonical encoding failed")]
    CanonicalEncoding,
    /// Input decoded but was not byte-for-byte canonical.
    #[error("provisional receipt encoding is not canonical")]
    NonCanonicalEncoding,
    /// The claim's overlay differs from the receiver or signing authority.
    #[error("provisional receipt network mismatch: expected {expected}, actual {actual}")]
    NetworkMismatch {
        /// Receiver or claim network.
        expected: u32,
        /// Presented network.
        actual: u32,
    },
    /// Provider and beneficiary cannot be the same account.
    #[error("provisional receipt provider and beneficiary are the same account")]
    SameAccountRoles,
    /// Probe has a unit count other than one.
    #[error("Probe requires one unit, got {units}")]
    InvalidProbeUnits {
        /// Rejected non-zero unit count.
        units: u64,
    },
    /// A delegated session belongs to a different account role.
    #[error("receipt signer account {actual} does not match role account {expected}")]
    SignerRoleMismatch {
        /// Account fixed by the claim role.
        expected: Did,
        /// Account recovered from the delegated session proof.
        actual: Did,
    },
    /// Provider signature failed under the provider domain.
    #[error("invalid provider receipt attestation")]
    InvalidProviderAttestation,
    /// Beneficiary signature failed under the beneficiary domain.
    #[error("invalid beneficiary receipt attestation")]
    InvalidBeneficiaryAttestation,
    /// Provider proof was not live at collection.
    #[error("provider receipt attestation was not live at collection")]
    ProviderAttestationNotLive,
    /// Beneficiary proof was not live at collection.
    #[error("beneficiary receipt attestation was not live at collection")]
    BeneficiaryAttestationNotLive,
    /// Millisecond observation time cannot be represented in whole seconds.
    #[error("receipt observation time exceeds the supported Unix-second range")]
    ObservationTimeOverflow,
    /// Claim slot is not the current or adjacent live-collection slot.
    #[error("receipt epoch {claim_slot} is outside tolerance around {observed_slot}")]
    EpochOutsideTolerance {
        /// Slot carried by the claim.
        claim_slot: u64,
        /// Receiver's current slot.
        observed_slot: u64,
    },
    /// Outer response and claim disagree about the account roles.
    #[error("probe transcript account roles are inconsistent")]
    TranscriptRoleMismatch,
    /// Embedded request transaction is invalid.
    #[error("probe request transaction is invalid")]
    InvalidRequestTransaction,
    /// Embedded completion transaction is invalid.
    #[error("probe completion transaction is invalid")]
    InvalidCompletionTransaction,
    /// A claim digest does not match its exact signed transaction.
    #[error("probe transcript digest mismatch")]
    TranscriptDigestMismatch,
}

fn encode_prefixed<T: Serialize>(
    prefix: &[u8],
    value: &T,
) -> std::result::Result<Vec<u8>, ServiceReceiptError> {
    let body = rings_codec::serialize(value).map_err(|_| ServiceReceiptError::CanonicalEncoding)?;
    let capacity = prefix
        .len()
        .checked_add(body.len())
        .ok_or(ServiceReceiptError::CanonicalEncoding)?;
    let mut bytes = Vec::with_capacity(capacity);
    bytes.extend_from_slice(prefix);
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

fn decode_prefixed<T>(prefix: &[u8], bytes: &[u8]) -> std::result::Result<T, ServiceReceiptError>
where T: serde::de::DeserializeOwned + Serialize {
    let body = bytes
        .strip_prefix(prefix)
        .ok_or(ServiceReceiptError::CanonicalEncoding)?;
    let value =
        rings_codec::deserialize(body).map_err(|_| ServiceReceiptError::CanonicalEncoding)?;
    if encode_prefixed(prefix, &value)? != bytes {
        return Err(ServiceReceiptError::NonCanonicalEncoding);
    }
    Ok(value)
}

#[cfg(test)]
mod tests;

// The native-only executable model explores time, loss, duplication,
// reordering, replay, restart, and eviction. Wasm keeps the shared refinement
// tests in `tests` without pulling in the native Stateright dependency.
#[cfg(all(test, not(target_family = "wasm")))]
mod model;
