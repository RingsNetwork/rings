//! Sender reservation and final-destination replay admission.

use std::num::NonZeroU64;
use std::ops::RangeInclusive;

use super::SwarmTransport;
use crate::dht::Did;
use crate::error::Result;
use crate::message::spent_at;
use crate::message::Admission;
use crate::message::LinkWindows;
use crate::message::MessageCategory;
use crate::message::OriginQuotaCharge;
use crate::message::OriginQuotaCounters;
use crate::message::Principal;
use crate::message::RelayAdmission;
use crate::message::RelayDelivery;
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

    /// Atomically commit final-destination replay and origin-quota admission of `transaction`,
    /// delivered by `introducer`.
    pub(crate) async fn admit_final_transaction(
        &self,
        transaction: &Transaction,
        charge: OriginQuotaCharge,
        introducer: Principal,
    ) -> Result<()> {
        let admission = self.admission(transaction, charge, introducer)?;
        self.transaction_replay
            .admit_with_quota(admission)
            .await
            .map(|_| ())
    }

    /// Admit a link-local transaction in `windows`, the volatile replay windows of the link it
    /// arrived on, with its origin-quota charge (#1081; see
    /// [`TransactionReplay::admit_on_link`]): nothing is written, and a refusal leaves its
    /// stream's window as it was.
    pub(crate) async fn admit_link_transaction(
        &self,
        windows: &mut LinkWindows,
        transaction: &Transaction,
        charge: OriginQuotaCharge,
    ) -> Result<()> {
        let key = transaction.stream_key(self.network_id)?;
        let digest = transaction.digest()?;
        let mut window = windows.remove(&key);
        let admitted = self
            .transaction_replay
            .admit_on_link(
                &mut window,
                key,
                transaction.sequence,
                digest,
                charge,
                logical_message_byte_cost(transaction),
            )
            .await;
        if let Some(window) = window {
            windows.insert(key, window);
        }
        admitted.map(|_| ())
    }

    /// The admission of `transaction` under `charge`, delivered by `introducer` (pure).
    fn admission(
        &self,
        transaction: &Transaction,
        charge: OriginQuotaCharge,
        introducer: Principal,
    ) -> Result<Admission> {
        let key = transaction.stream_key(self.network_id)?;
        let proof = &transaction.verification;
        Ok(Admission {
            key,
            sequence: transaction.sequence,
            digest: transaction.digest()?,
            charge,
            byte_cost: logical_message_byte_cost(transaction),
            introducer,
            spent_at_ms: spent_at(key.class, proof.ts_ms, proof.ttl_ms),
        })
    }

    /// Atomically commit final-destination replay, relay custody and origin-quota admission of
    /// a transaction a relay inbox delivered (the custody law of `message::replay`): either it
    /// enters the window and custody in one record write, or it is already in custody and is
    /// queued again with nothing admitted. The delivery is out of this node's own relay inbox,
    /// so a new stream is local.
    pub(crate) async fn admit_relayed_transaction(
        &self,
        transaction: &Transaction,
        charge: OriginQuotaCharge,
    ) -> Result<RelayAdmission> {
        let admission = self.admission(transaction, charge, Principal::Local)?;
        self.transaction_replay.admit_relayed(admission).await
    }

    /// Release a relay-delivered transaction from custody once the application took it.
    pub(crate) async fn release_relayed_transaction(
        &self,
        transaction: &Transaction,
    ) -> Result<()> {
        let (key, delivery) = self.relay_delivery(transaction)?;
        self.transaction_replay.release_relayed(key, delivery).await
    }

    /// Restrict relay custody to the transactions of the elements still in the inbox carrier.
    pub(crate) async fn restrict_relay_custody<'a>(
        &self,
        stored: impl IntoIterator<Item = &'a Transaction>,
    ) -> Result<()> {
        let stored = stored
            .into_iter()
            .map(|transaction| self.relay_delivery(transaction))
            .collect::<Result<_>>()?;
        self.transaction_replay.restrict_custody(stored).await
    }

    /// The stream and the custody identity of `transaction` (pure).
    fn relay_delivery(&self, transaction: &Transaction) -> Result<(StreamKey, RelayDelivery)> {
        Ok((transaction.stream_key(self.network_id)?, RelayDelivery {
            sequence: transaction.sequence,
            digest: transaction.digest()?,
        }))
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
