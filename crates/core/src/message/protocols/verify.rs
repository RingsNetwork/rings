#![deny(missing_docs)]

//! Implementation of Message Verification.
//!
//! A [`MessageVerification`] is a session signature over a *domain-separated* transcript:
//!
//! ```text
//! transcript(d, ts, ttl, m) = len(tag(d)) || tag(d) || network_id(d) || ts || ttl || m
//! ```
//!
//! where `d : SigningDomain = (tag, network_id)` names the message family and the overlay, all
//! integers are big-endian, and `len(tag)` is one byte. The length prefix makes the tag component
//! prefix-free, so transcripts of distinct domains never collide for any `m`. Binding `network_id`
//! makes a signature non-portable across overlays that share a session key; binding the tag makes
//! it non-portable across message families that share a signing surface.
//!
//! Authority: every signature is issued by a [`MessageSigner`], a session key paired with the
//! overlay it acts in, and every verification is performed against the *receiver's* domain, never
//! against a value carried in the message. A type that embeds a verification implements
//! [`MessageVerificationExt`] with its own [`DomainTag`] and verifies under the receiver's
//! `network_id`.

use std::borrow::Borrow;

use serde::Deserialize;
use serde::Serialize;

use crate::consts::DEFAULT_TTL_MS;
use crate::consts::MAX_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::Did;
use crate::error::Result;
use crate::session::Session;
use crate::session::SessionSk;
use crate::utils::get_epoch_ms;

/// Name of one signed message family; the first component of a [`SigningDomain`].
///
/// Law: `label.len() <= u8::MAX`, so the label fits its one-byte length prefix in the
/// transcript. Construct tags with [`domain_tag!`](crate::domain_tag), which checks the law at
/// compile time; [`DomainTag::new`] is the total form for labels known only at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DomainTag(&'static str);

impl DomainTag {
    /// Name a message family, or `None` when `label` does not fit its length prefix.
    pub const fn new(label: &'static str) -> Option<Self> {
        if label.len() <= u8::MAX as usize {
            Some(Self(label))
        } else {
            None
        }
    }

    /// The label bytes.
    pub const fn as_bytes(self) -> &'static [u8] {
        self.0.as_bytes()
    }

    /// The label as one byte, valid by the constructor law.
    const fn len_byte(self) -> u8 {
        self.0.len() as u8
    }
}

/// Name a message family from a literal, rejecting at compile time a label longer than the
/// one-byte length prefix allows.
#[macro_export]
macro_rules! domain_tag {
    ($label:literal) => {{
        const _: () = assert!(
            $label.len() <= u8::MAX as usize,
            "domain tag label must fit its one-byte length prefix"
        );
        const TAG: $crate::message::DomainTag = match $crate::message::DomainTag::new($label) {
            Some(tag) => tag,
            // Excluded by the assertion above.
            None => unreachable!(),
        };
        TAG
    }};
}

/// The message family and overlay a signature transcript is bound to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SigningDomain {
    tag: DomainTag,
    network_id: u32,
}

impl SigningDomain {
    /// Bind a message family to an overlay.
    pub const fn new(tag: DomainTag, network_id: u32) -> Self {
        Self { tag, network_id }
    }

    /// The message family.
    pub const fn tag(self) -> DomainTag {
        self.tag
    }

    /// The overlay.
    pub const fn network_id(self) -> u32 {
        self.network_id
    }

    /// The signed bytes for `data` stamped `(ts_ms, ttl_ms)` under this domain.
    ///
    /// Law (injectivity): `transcript(d, ts, ttl, m) = transcript(d', ts', ttl', m')` implies
    /// `(d, ts, ttl, m) = (d', ts', ttl', m')`, because every component before `m` has a fixed
    /// width or a length prefix.
    pub(crate) fn transcript(self, data: &[u8], ts_ms: u128, ttl_ms: u64) -> Vec<u8> {
        let tag = self.tag.as_bytes();
        let mut msg = Vec::with_capacity(1 + tag.len() + 4 + 16 + 8 + data.len());

        msg.push(self.tag.len_byte());
        msg.extend_from_slice(tag);
        msg.extend_from_slice(&self.network_id.to_be_bytes());
        msg.extend_from_slice(&ts_ms.to_be_bytes());
        msg.extend_from_slice(&ttl_ms.to_be_bytes());
        msg.extend_from_slice(data);

        msg
    }
}

/// A session key acting inside one overlay: the authority that issues every
/// [`MessageVerification`] a node signs.
///
/// Signing is the map `(session_sk, network_id) × (tag, data) → MessageVerification`. This
/// type fixes the first component, so message constructors take one authority instead of a key
/// and an overlay that could be paired inconsistently. `S` is how the key is held: a borrowed
/// authority `MessageSigner<&SessionSk>` is `Copy` and is what signing functions take; an owned
/// authority `MessageSigner<SessionSk>` is what long-lived runtimes store, so the pairing is
/// never split across two fields.
///
/// Law: copying or cloning an authority is semantically invisible, since it names the same key
/// and the same overlay.
#[derive(Clone, Copy, Debug)]
pub struct MessageSigner<S> {
    session_sk: S,
    network_id: u32,
}

impl<S> MessageSigner<S>
where S: Borrow<SessionSk>
{
    /// Let `session_sk` sign on behalf of the overlay `network_id`.
    pub const fn new(session_sk: S, network_id: u32) -> Self {
        Self {
            session_sk,
            network_id,
        }
    }

    /// The session key.
    pub fn session_sk(&self) -> &SessionSk {
        self.session_sk.borrow()
    }

    /// The overlay this authority signs for.
    pub const fn network_id(&self) -> u32 {
        self.network_id
    }

    /// The account DID that authorized the session key.
    pub fn account_did(&self) -> Did {
        self.session_sk().account_did()
    }

    /// This authority borrowing its key: the form signing functions take.
    pub fn by_ref(&self) -> MessageSigner<&SessionSk> {
        MessageSigner::new(self.session_sk(), self.network_id)
    }

    /// This authority owning a copy of its key: the form long-lived runtimes store.
    pub fn owned(&self) -> MessageSigner<SessionSk> {
        MessageSigner::new(self.session_sk().clone(), self.network_id)
    }

    /// Sign `data` as a member of the message family `tag` inside this overlay, stamped with
    /// the current time and the default TTL.
    pub fn sign(&self, tag: DomainTag, data: &[u8]) -> Result<MessageVerification> {
        let domain = SigningDomain::new(tag, self.network_id);
        let ts_ms = get_epoch_ms();
        let ttl_ms = DEFAULT_TTL_MS;
        let msg = domain.transcript(data, ts_ms, ttl_ms);
        Ok(MessageVerification {
            session: self.session_sk().session(),
            sig: self.session_sk().sign(&msg)?,
            ttl_ms,
            ts_ms,
        })
    }
}

/// Message Verification is based on session, and sig.
/// it also included ttl time and created ts.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct MessageVerification {
    /// The [Session] of the [SessionSk]. Used to identify a sender and verify the signature.
    pub session: Session,
    /// The time to live of the message in milliseconds.
    pub ttl_ms: u64,
    /// The timestamp of the message in milliseconds.
    pub ts_ms: u128,
    /// The signature of the message. Signed by [SessionSk]. Can be verified by [Session].
    pub sig: Vec<u8>,
}

impl MessageVerification {
    /// Verify the signature over `data` under the receiver's `domain`, with the signing session
    /// judged as of now.
    pub fn verify(&self, domain: SigningDomain, data: &[u8]) -> bool {
        self.verify_at(domain, data, get_epoch_ms())
    }

    /// Verify the signature over `data` under the receiver's `domain`, with the signing session
    /// judged as of the instant `at_ms`.
    ///
    /// Post: `true` implies the session was authorized and live at `at_ms` and signed the
    /// transcript of `(domain, data, ts_ms, ttl_ms)`. Proof liveness is not judged here; see
    /// [`Self::is_live_at`].
    pub fn verify_at(&self, domain: SigningDomain, data: &[u8], at_ms: u128) -> bool {
        let msg = domain.transcript(data, self.ts_ms, self.ttl_ms);

        self.session
            .verify_at(&msg, &self.sig, at_ms)
            .map_err(|e| {
                tracing::warn!("MessageVerification verify failed: {:?}", e);
            })
            .is_ok()
    }

    /// Return whether the verification timestamp is outside its accepted lifetime.
    pub fn is_expired(&self) -> bool {
        !self.is_live_at(get_epoch_ms())
    }

    /// Return whether the verification timestamp and TTL describe a currently live proof.
    ///
    /// Pre: `now_ms` is the receiver's current wall-clock time.
    /// Post: `true` implies `ttl_ms <= MAX_TTL_MS`, the timestamp is not beyond the accepted future
    /// skew, and `now_ms` has not passed `ts_ms + ttl_ms`.
    pub fn is_live_at(&self, now_ms: u128) -> bool {
        self.ttl_ms <= MAX_TTL_MS
            && self.ts_ms.saturating_sub(TS_OFFSET_TOLERANCE_MS) <= now_ms
            && now_ms <= self.ts_ms.saturating_add(self.ttl_ms as u128)
    }

    /// Verify the signature only when the verification timestamp is still live.
    pub fn verify_unexpired(&self, domain: SigningDomain, data: &[u8]) -> bool {
        if self.is_expired() {
            tracing::warn!("message expired");
            return false;
        }

        self.verify(domain, data)
    }
}

/// This trait helps a struct with `MessageVerification` field to `verify` itself.
/// It also provides a `signer` method to let receiver know who sent the message.
pub trait MessageVerificationExt {
    /// The message family this type is signed under. Paired with the receiver's overlay it
    /// fixes the [`SigningDomain`] of [`Self::verification`].
    const DOMAIN_TAG: DomainTag;

    /// Give the data to be verified.
    fn verification_data(&self) -> Result<Vec<u8>>;

    /// Give the verification field for verifying.
    fn verification(&self) -> &MessageVerification;

    /// Checks whether the message is expired.
    fn is_expired(&self) -> bool {
        self.verification().is_expired()
    }

    /// Verifies the message as of the instant `at_ms`: its proof was live then, its session was
    /// authorized and live then, and the signature was issued for this message family inside
    /// the overlay `network_id`.
    ///
    /// Every transport path judges as of now through [`Self::verify`]; a relay inbox judges a
    /// held message as of the instant its holder received it, so a sender's session expiring
    /// afterwards does not retroactively unverify a message that was valid when held.
    fn verify_at(&self, network_id: u32, at_ms: u128) -> bool {
        if !self.verification().is_live_at(at_ms) {
            tracing::warn!("message proof not live at the judged instant");
            return false;
        }
        let Ok(data) = self.verification_data() else {
            tracing::warn!("MessageVerificationExt verify get verification_data failed");
            return false;
        };

        self.verification().verify_at(
            SigningDomain::new(Self::DOMAIN_TAG, network_id),
            &data,
            at_ms,
        )
    }

    /// Verifies that the message is not expired and that the signature was issued for this
    /// message family inside the overlay `network_id`, as of now.
    fn verify(&self, network_id: u32) -> bool {
        self.verify_at(network_id, get_epoch_ms())
    }

    /// Get signer did from verification.
    fn signer(&self) -> Did {
        self.verification().session.account_did()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;

    const FIXTURE_TAG: DomainTag = domain_tag!("rings-core:test:fixture:v1");
    const OTHER_TAG: DomainTag = domain_tag!("rings-core:test:other:v1");
    const NETWORK_ID: u32 = 7;

    struct VerifiedFixture {
        verification: MessageVerification,
    }

    impl MessageVerificationExt for VerifiedFixture {
        const DOMAIN_TAG: DomainTag = FIXTURE_TAG;

        fn verification_data(&self) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }

        fn verification(&self) -> &MessageVerification {
            &self.verification
        }
    }

    fn fixture_domain() -> SigningDomain {
        SigningDomain::new(FIXTURE_TAG, NETWORK_ID)
    }

    fn fixture_signer(session_sk: &SessionSk) -> MessageSigner<&SessionSk> {
        MessageSigner::new(session_sk, NETWORK_ID)
    }

    #[test]
    fn test_expiration_handles_timestamp_below_tolerance_without_underflow() -> Result<()> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let mut verification = fixture_signer(&session_sk).sign(FIXTURE_TAG, &[])?;
        verification.ts_ms = 0;
        let fixture = VerifiedFixture { verification };

        assert!(fixture.is_expired());
        Ok(())
    }

    fn signed_verification(
        data: &[u8],
        session_sk: &SessionSk,
        ts_ms: u128,
        ttl_ms: u64,
    ) -> Result<MessageVerification> {
        let msg = fixture_domain().transcript(data, ts_ms, ttl_ms);
        Ok(MessageVerification {
            session: session_sk.session(),
            ttl_ms,
            ts_ms,
            sig: session_sk.sign(&msg)?,
        })
    }

    #[test]
    fn test_verify_unexpired_rejects_ttl_above_max() -> Result<()> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let proof = signed_verification(&[], &session_sk, get_epoch_ms(), MAX_TTL_MS + 1)?;

        assert!(proof.is_expired());
        assert!(!proof.verify_unexpired(fixture_domain(), &[]));
        Ok(())
    }

    #[test]
    fn test_verify_unexpired_rejects_timestamp_beyond_future_tolerance() -> Result<()> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let proof = signed_verification(
            &[],
            &session_sk,
            get_epoch_ms() + TS_OFFSET_TOLERANCE_MS + 60_000,
            1_000,
        )?;

        assert!(proof.is_expired());
        assert!(!proof.verify_unexpired(fixture_domain(), &[]));
        Ok(())
    }

    /// Law: the transcript is the length-prefixed tag, the overlay, the stamp, then the data.
    #[test]
    fn test_transcript_layout_is_length_prefixed_tag_then_overlay_then_stamp() {
        let transcript = fixture_domain().transcript(b"data", 3, 5);
        let tag = FIXTURE_TAG.as_bytes();

        let mut expected = vec![tag.len() as u8];
        expected.extend_from_slice(tag);
        expected.extend_from_slice(&NETWORK_ID.to_be_bytes());
        expected.extend_from_slice(&3u128.to_be_bytes());
        expected.extend_from_slice(&5u64.to_be_bytes());
        expected.extend_from_slice(b"data");
        assert_eq!(transcript, expected);
    }

    /// Law: a label longer than the one-byte length prefix is not a tag.
    #[test]
    fn test_domain_tag_rejects_label_beyond_length_prefix() {
        const LONG: &str = "rings-core:test:long-label:v1 - 0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789";
        assert!(LONG.len() > usize::from(u8::MAX));
        assert!(DomainTag::new(LONG).is_none());
        assert!(DomainTag::new("rings-core:test:short:v1").is_some());
    }

    /// Law: a signature issued for overlay `n` does not verify under overlay `n' != n`.
    #[test]
    fn test_signature_is_bound_to_the_overlay() -> Result<()> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let verification = fixture_signer(&session_sk).sign(FIXTURE_TAG, b"data")?;

        assert!(verification.verify(fixture_domain(), b"data"));
        assert!(!verification.verify(SigningDomain::new(FIXTURE_TAG, NETWORK_ID + 1), b"data"));
        Ok(())
    }

    /// Law: a signature issued for message family `t` does not verify under `t' != t`.
    #[test]
    fn test_signature_is_bound_to_the_message_family() -> Result<()> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let verification = fixture_signer(&session_sk).sign(FIXTURE_TAG, b"data")?;

        assert!(!verification.verify(SigningDomain::new(OTHER_TAG, NETWORK_ID), b"data"));
        Ok(())
    }

    /// Law: `MessageVerificationExt::verify` checks the type's own tag against the receiver's
    /// overlay, so a fixture signed elsewhere is rejected even with identical data.
    #[test]
    fn test_ext_verify_uses_type_tag_and_receiver_overlay() -> Result<()> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let signer = fixture_signer(&session_sk);
        let fixture = VerifiedFixture {
            verification: signer.sign(FIXTURE_TAG, &[])?,
        };

        assert!(fixture.verify(NETWORK_ID));
        assert!(!fixture.verify(NETWORK_ID + 1));

        let mislabeled = VerifiedFixture {
            verification: signer.sign(OTHER_TAG, &[])?,
        };
        assert!(!mislabeled.verify(NETWORK_ID));
        Ok(())
    }

    /// Law: the owned and borrowed forms of one authority sign identically verifiable proofs.
    #[test]
    fn test_owned_and_borrowed_authorities_are_the_same_signer() -> Result<()> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let owned = fixture_signer(&session_sk).owned();
        let proof = owned.by_ref().sign(FIXTURE_TAG, b"data")?;

        assert_eq!(owned.network_id(), NETWORK_ID);
        assert_eq!(owned.account_did(), session_sk.account_did());
        assert!(proof.verify(fixture_domain(), b"data"));
        Ok(())
    }
}
