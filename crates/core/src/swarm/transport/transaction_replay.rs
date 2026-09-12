//! Sender reservation and final-destination replay admission.

use std::num::NonZeroU64;
use std::ops::RangeInclusive;

use super::SwarmTransport;
use crate::dht::Did;
use crate::error::Result;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::message::ReplayCounters;
use crate::message::StreamKey;
use crate::message::Transaction;

impl SwarmTransport {
    /// Current destination-scoped replay rejection and persistence counters.
    pub(crate) fn replay_counters(&self) -> ReplayCounters {
        self.transaction_replay.counters()
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

    /// Persist the final destination's replay transition before application validation.
    pub(crate) async fn admit_transaction_replay(&self, transaction: &Transaction) -> Result<()> {
        let key = transaction.stream_key(self.network_id);
        let digest = transaction.digest()?;
        self.transaction_replay
            .admit(key, transaction.sequence, digest)
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
