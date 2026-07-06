#![warn(missing_docs)]

//! Implementation of Message Verification.

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

fn pack_msg(data: &[u8], ts_ms: u128, ttl_ms: u64) -> Vec<u8> {
    let mut msg = vec![];

    msg.extend_from_slice(&ts_ms.to_be_bytes());
    msg.extend_from_slice(&ttl_ms.to_be_bytes());
    msg.extend_from_slice(data);

    msg
}

impl MessageVerification {
    /// Create a new MessageVerification. Should provide the data and the [SessionSk].
    pub fn new(data: &[u8], session_sk: &SessionSk) -> Result<Self> {
        let ts_ms = get_epoch_ms();
        let ttl_ms = DEFAULT_TTL_MS;
        let msg = pack_msg(data, ts_ms, ttl_ms);
        let verification = MessageVerification {
            session: session_sk.session(),
            sig: session_sk.sign(&msg)?,
            ttl_ms,
            ts_ms,
        };
        Ok(verification)
    }

    /// Verify a MessageVerification
    pub fn verify(&self, data: &[u8]) -> bool {
        let msg = pack_msg(data, self.ts_ms, self.ttl_ms);

        self.session
            .verify(&msg, &self.sig)
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
    pub fn verify_unexpired(&self, data: &[u8]) -> bool {
        if self.is_expired() {
            tracing::warn!("message expired");
            return false;
        }

        self.verify(data)
    }
}

/// This trait helps a struct with `MessageVerification` field to `verify` itself.
/// It also provides a `signer` method to let receiver know who sent the message.
pub trait MessageVerificationExt {
    /// Give the data to be verified.
    fn verification_data(&self) -> Result<Vec<u8>>;

    /// Give the verification field for verifying.
    fn verification(&self) -> &MessageVerification;

    /// Checks whether the message is expired.
    fn is_expired(&self) -> bool {
        self.verification().is_expired()
    }

    /// Verifies that the message is not expired and that the signature is valid.
    fn verify(&self) -> bool {
        if self.is_expired() {
            tracing::warn!("message expired");
            return false;
        }

        let Ok(data) = self.verification_data() else {
            tracing::warn!("MessageVerificationExt verify get verification_data failed");
            return false;
        };

        self.verification().verify_unexpired(&data)
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

    struct VerifiedFixture {
        verification: MessageVerification,
    }

    impl MessageVerificationExt for VerifiedFixture {
        fn verification_data(&self) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }

        fn verification(&self) -> &MessageVerification {
            &self.verification
        }
    }

    #[test]
    fn expiration_handles_timestamp_below_tolerance_without_underflow() -> Result<()> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let mut verification = MessageVerification::new(&[], &session_sk)?;
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
        let msg = pack_msg(data, ts_ms, ttl_ms);
        Ok(MessageVerification {
            session: session_sk.session(),
            ttl_ms,
            ts_ms,
            sig: session_sk.sign(&msg)?,
        })
    }

    #[test]
    fn verify_unexpired_rejects_ttl_above_max() -> Result<()> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let proof = signed_verification(&[], &session_sk, get_epoch_ms(), MAX_TTL_MS + 1)?;

        assert!(proof.is_expired());
        assert!(!proof.verify_unexpired(&[]));
        Ok(())
    }

    #[test]
    fn verify_unexpired_rejects_timestamp_beyond_future_tolerance() -> Result<()> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let proof = signed_verification(
            &[],
            &session_sk,
            get_epoch_ms() + TS_OFFSET_TOLERANCE_MS + 60_000,
            1_000,
        )?;

        assert!(proof.is_expired());
        assert!(!proof.verify_unexpired(&[]));
        Ok(())
    }
}
