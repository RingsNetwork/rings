//! Sender reservation and final-destination replay admission.

use std::num::NonZeroU64;
use std::ops::RangeInclusive;

use super::SwarmTransport;
use crate::dht::Did;
use crate::error::Result;
use crate::message::MessageCategory;
use crate::message::OriginQuotaCharge;
use crate::message::OriginQuotaCounters;
use crate::message::PayloadSender;
use crate::message::ReplayCounters;
use crate::message::StreamKey;
use crate::message::Transaction;

impl SwarmTransport {
    /// Current destination-scoped replay rejection and persistence counters.
    pub(crate) fn replay_counters(&self) -> ReplayCounters {
        self.transaction_replay.counters()
    }

    /// Current final-destination origin-quota rejection counters.
    pub(crate) fn origin_quota_counters(&self) -> OriginQuotaCounters {
        self.transaction_replay.quota_counters()
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) async fn origin_quota_record_count_for_test(&self) -> usize {
        self.transaction_replay.quota_record_count_for_test().await
    }

    /// The whole `(message, byte)` tokens of `origin`'s record in `lane` at this destination,
    /// as of its last admission: a stored record is not refilled until it is next admitted.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn origin_quota_tokens_for_test(
        &self,
        origin: Did,
        lane: crate::message::MessageCategory,
    ) -> Option<(u128, u128)> {
        let key = crate::message::OriginQuotaKey::new(self.network_id, origin, self.dht.did, lane);
        self.transaction_replay.quota_tokens_for_test(key).await
    }

    /// Persistently reserve one or more sequences of `class` for this account and final
    /// destination.
    pub(crate) async fn reserve_transaction_sequences(
        &self,
        destination: Did,
        class: MessageCategory,
        count: NonZeroU64,
    ) -> Result<RangeInclusive<u64>> {
        let key = StreamKey::new(
            self.network_id,
            self.message_signer().delegator_did(),
            destination,
            class,
        );
        self.transaction_replay.reserve(key, count).await
    }

    /// Atomically commit final-destination replay and origin-quota admission.
    pub(crate) async fn admit_final_transaction(
        &self,
        transaction: &Transaction,
        charge: OriginQuotaCharge,
    ) -> Result<()> {
        let key = transaction.stream_key(self.network_id)?;
        let digest = transaction.digest()?;
        self.transaction_replay
            .admit_with_quota(
                key,
                transaction.sequence,
                digest,
                charge,
                logical_message_byte_cost(transaction),
            )
            .await
            .map(|_| ())
    }
}

/// Deterministic quota cost of one verified logical transaction.
///
/// Normal frames use their signed transaction data directly. Chunk envelopes are never passed to
/// this function; after reassembly, the recovered original transaction enters the same function
/// once, so chunk count and envelope overhead neither evade nor multiply the charge.
fn logical_message_byte_cost(transaction: &Transaction) -> usize {
    transaction.data.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::DelegateeKey;
    use crate::ecc::SecretKey;
    use crate::message::Message;
    use crate::message::MessageSigner;

    #[test]
    fn logical_byte_cost_is_the_signed_message_data_length() -> Result<()> {
        let session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        let transaction = Transaction::new(
            SecretKey::random().address().into(),
            uuid::Uuid::new_v4(),
            0,
            None,
            Message::custom(b"logical bytes")?,
            MessageSigner::new(&session, 7),
        )?;

        assert_eq!(
            logical_message_byte_cost(&transaction),
            transaction.data.len()
        );
        Ok(())
    }
}
