//! Sender reservation and final-destination replay admission.

use std::num::NonZeroU64;
use std::ops::RangeInclusive;

use super::SwarmTransport;
use crate::dht::Did;
use crate::error::Result;
use crate::message::MessagePayload;
use crate::message::OriginQuotaCounters;
use crate::message::OriginQuotaLane;
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

    #[cfg(test)]
    pub(crate) async fn origin_quota_record_count_for_test(&self) -> usize {
        self.transaction_replay.quota_record_count_for_test().await
    }

    /// Persistently reserve one or more sequences for this account and final destination.
    pub(crate) async fn reserve_transaction_sequences(
        &self,
        destination: Did,
        count: NonZeroU64,
    ) -> Result<RangeInclusive<u64>> {
        let key = StreamKey::new(
            self.network_id,
            self.message_signer().account_did(),
            destination,
        );
        self.transaction_replay.reserve(key, count).await
    }

    /// Atomically commit final-destination replay and origin-quota admission.
    pub(crate) async fn admit_final_transaction(
        &self,
        transaction: &Transaction,
        lane: OriginQuotaLane,
    ) -> Result<()> {
        let key = transaction.stream_key(self.network_id);
        let digest = transaction.digest()?;
        self.transaction_replay
            .admit_with_quota(
                key,
                transaction.sequence,
                digest,
                lane,
                logical_message_byte_cost(transaction),
            )
            .await
            .map(|_| ())
    }

    /// Build a locally originated payload after durably reserving its stream sequence.
    pub(crate) async fn signed_payload<T>(
        &self,
        data: T,
        next_hop: Did,
        destination: Did,
    ) -> Result<MessagePayload>
    where
        T: serde::Serialize,
    {
        let sequence = *self
            .reserve_transaction_sequences(destination, NonZeroU64::MIN)
            .await?
            .start();
        MessagePayload::new_send_with_sequence(
            data,
            self.message_signer(),
            next_hop,
            destination,
            sequence,
        )
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
    use crate::ecc::SecretKey;
    use crate::message::Message;
    use crate::message::MessageSigner;
    use crate::session::SessionSk;

    #[test]
    fn logical_byte_cost_is_the_signed_message_data_length() -> Result<()> {
        let session = SessionSk::new_with_seckey(&SecretKey::random())?;
        let transaction = Transaction::new(
            SecretKey::random().address().into(),
            uuid::Uuid::new_v4(),
            0,
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
