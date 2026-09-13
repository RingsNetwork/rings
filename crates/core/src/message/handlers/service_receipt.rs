//! ProbeV1 provisional service-receipt state-machine handlers.

use std::num::NonZeroU64;

use async_trait::async_trait;

use crate::error::Error;
use crate::error::Result;
use crate::message::effects::CoreEffect;
use crate::message::HandleMsg;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::message::ProbeAcknowledgementV1;
use crate::message::ProbeCompletionV1;
use crate::message::ProbeOfferV1;
use crate::message::ProbeRequestV1;
use crate::message::ProvisionalEpochV1;
use crate::message::ProvisionalServiceClaimV1;
use crate::message::ProvisionalServiceReceiptV1;
use crate::message::ServiceReceiptError;
use crate::message::Transaction;
use crate::utils::get_epoch_ms;

/// ProbeV1 request is a direct authenticated overlay liveness probe.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ProbeRequestV1> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &ProbeRequestV1) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }

        let now_seconds = u64::try_from(get_epoch_ms() / 1_000)
            .map_err(|_| ServiceReceiptError::ObservationTimeOverflow)?;
        if !msg.epoch.is_accepted_at(now_seconds) {
            return Err(ServiceReceiptError::EpochOutsideTolerance {
                claim_slot: msg.epoch.slot,
                observed_slot: ProvisionalEpochV1::from_unix_seconds(now_seconds).slot,
            }
            .into());
        }
        let beneficiary = ctx.transaction.origin();
        let provider = self.dht.did;
        let network_id = self.transport.network_id;
        let request_digest = ctx.transaction.digest()?.into_bytes();
        let completion_sequence = *self
            .transport
            .reserve_transaction_sequences(beneficiary, NonZeroU64::MIN)
            .await?
            .start();
        let signer = self.transport.message_signer();
        let completion = Transaction::new(
            beneficiary,
            ctx.transaction.tx_id,
            completion_sequence,
            ProbeCompletionV1 {
                request_digest,
                nonce: msg.nonce,
            },
            signer,
        )?;
        let claim = ProvisionalServiceClaimV1::probe(
            network_id,
            provider,
            beneficiary,
            msg.epoch,
            msg.nonce,
            request_digest,
            completion.digest()?.into_bytes(),
        );
        let provider_attestation = claim.sign_provider(signer)?;
        self.run_effects([CoreEffect::send_report_message(
            ctx,
            Message::ProbeOfferV1(Box::new(ProbeOfferV1 {
                request: ctx.transaction.clone(),
                completion,
                claim,
                provider_attestation,
            })),
        )])
        .await
    }
}

/// Beneficiary verifies an offered transcript and returns its role attestation once.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ProbeOfferV1> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &ProbeOfferV1) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }

        let beneficiary = self.dht.did;
        let provider = ctx.transaction.origin();
        let request = msg.verify_live_transcript_at(
            ctx,
            self.transport.network_id,
            beneficiary,
            get_epoch_ms(),
        )?;
        if !self
            .transport
            .consume_pending_probe(provider, ctx.transaction.tx_id, request)?
        {
            return Err(Error::InvalidMessage(
                "probe offer has no matching outstanding request".to_string(),
            ));
        }
        let beneficiary_attestation = msg
            .claim
            .sign_beneficiary(self.transport.message_signer())?;
        let receipt = ProvisionalServiceReceiptV1::new(
            msg.claim.clone(),
            msg.provider_attestation.clone(),
            beneficiary_attestation,
        )?;
        self.run_effects([CoreEffect::send_report_message(
            ctx,
            Message::ProbeAcknowledgementV1(Box::new(ProbeAcknowledgementV1 { receipt })),
        )])
        .await
    }
}

/// Provider verifies the acknowledgement before admitting bounded evidence.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ProbeAcknowledgementV1> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &ProbeAcknowledgementV1) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        if msg.receipt.claim.provider_account != self.dht.did
            || msg.receipt.claim.beneficiary_account != ctx.transaction.origin()
        {
            return Err(ServiceReceiptError::TranscriptRoleMismatch.into());
        }
        self.transport
            .admit_provisional_receipt(&msg.receipt, get_epoch_ms())
            .await
    }
}
