//! Probe provisional service-receipt state-machine handlers.
//!
//! Pure planners construct transcript values. [`ProbeEffect`] names external work, and
//! [`ProbeEffectInterpreter`] is the only adapter that reads clocks or mutates transport state.

use std::num::NonZeroU64;

use async_trait::async_trait;

use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::effects::CoreEffect;
use crate::message::HandleMsg;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::PayloadSender;
use crate::message::ProbeAcknowledgement;
use crate::message::ProbeCompletion;
use crate::message::ProbeOffer;
use crate::message::ProbeRequest;
use crate::message::ProvisionalEpoch;
use crate::message::ProvisionalServiceClaim;
use crate::message::ProvisionalServiceReceipt;
use crate::message::ServiceReceiptError;
use crate::message::Transaction;
use crate::session::SessionSk;
use crate::utils::get_epoch_ms;

enum ProbeEffect<'payload> {
    Forward {
        ctx: &'payload MessagePayload,
    },
    RespondToRequest {
        ctx: &'payload MessagePayload,
        request: &'payload ProbeRequest,
    },
    RespondToOffer {
        ctx: &'payload MessagePayload,
        offer: &'payload ProbeOffer,
    },
    AdmitAcknowledgement {
        ctx: &'payload MessagePayload,
        acknowledgement: &'payload ProbeAcknowledgement,
    },
}

impl<'payload> ProbeEffect<'payload> {
    fn local_or_forward(local: Did, ctx: &'payload MessagePayload, local_effect: Self) -> Self {
        match ctx.should_forward_from(local).then_some(ctx) {
            Some(ctx) => Self::Forward { ctx },
            None => local_effect,
        }
    }

    fn request(local: Did, ctx: &'payload MessagePayload, request: &'payload ProbeRequest) -> Self {
        Self::local_or_forward(local, ctx, Self::RespondToRequest { ctx, request })
    }

    fn offer(local: Did, ctx: &'payload MessagePayload, offer: &'payload ProbeOffer) -> Self {
        Self::local_or_forward(local, ctx, Self::RespondToOffer { ctx, offer })
    }

    fn acknowledgement(
        local: Did,
        ctx: &'payload MessagePayload,
        acknowledgement: &'payload ProbeAcknowledgement,
    ) -> Self {
        Self::local_or_forward(local, ctx, Self::AdmitAcknowledgement {
            ctx,
            acknowledgement,
        })
    }
}

struct ProbeOfferPlan {
    request: Transaction,
    provider: Did,
    beneficiary: Did,
    network_id: u32,
    epoch: ProvisionalEpoch,
    nonce: [u8; 32],
    request_digest: [u8; 32],
}

impl ProbeOfferPlan {
    fn build(
        self,
        completion_sequence: u64,
        signer: MessageSigner<&SessionSk>,
    ) -> Result<ProbeOffer> {
        let completion = Transaction::new(
            self.beneficiary,
            self.request.tx_id,
            completion_sequence,
            ProbeCompletion {
                request_digest: self.request_digest,
                nonce: self.nonce,
            },
            signer,
        )?;
        let claim = ProvisionalServiceClaim::probe(
            self.network_id,
            self.provider,
            self.beneficiary,
            self.epoch,
            self.nonce,
            self.request_digest,
            completion.digest()?.into_bytes(),
        );
        let provider_attestation = claim.sign_provider(signer)?;
        Ok(ProbeOffer {
            request: self.request,
            completion,
            claim,
            provider_attestation,
        })
    }
}

fn plan_probe_offer(
    ctx: &MessagePayload,
    request: &ProbeRequest,
    provider: Did,
    network_id: u32,
    observed_at_ms: u128,
) -> Result<ProbeOfferPlan> {
    let observed_seconds = u64::try_from(observed_at_ms / 1_000)
        .map_err(|_| ServiceReceiptError::ObservationTimeOverflow)?;
    match request.epoch.is_accepted_at(observed_seconds).then_some(()) {
        Some(()) => Ok(()),
        None => Err(ServiceReceiptError::EpochOutsideTolerance {
            claim_slot: request.epoch.slot,
            observed_slot: ProvisionalEpoch::from_unix_seconds(observed_seconds).slot,
        }),
    }?;

    Ok(ProbeOfferPlan {
        request: ctx.transaction.clone(),
        provider,
        beneficiary: ctx.transaction.origin(),
        network_id,
        epoch: request.epoch,
        nonce: request.nonce,
        request_digest: ctx.transaction.digest()?.into_bytes(),
    })
}

fn build_probe_acknowledgement(
    offer: &ProbeOffer,
    signer: MessageSigner<&SessionSk>,
) -> Result<ProbeAcknowledgement> {
    let beneficiary_attestation = offer.claim.sign_beneficiary(signer)?;
    let receipt = ProvisionalServiceReceipt::new(
        offer.claim.clone(),
        offer.provider_attestation.clone(),
        beneficiary_attestation,
    )?;
    Ok(ProbeAcknowledgement { receipt })
}

fn validate_acknowledgement_roles(
    acknowledgement: &ProbeAcknowledgement,
    provider: Did,
    beneficiary: Did,
) -> Result<&ProvisionalServiceReceipt> {
    match (acknowledgement.receipt.claim.provider_account == provider
        && acknowledgement.receipt.claim.beneficiary_account == beneficiary)
        .then_some(&acknowledgement.receipt)
    {
        Some(receipt) => Ok(receipt),
        None => Err(ServiceReceiptError::TranscriptRoleMismatch.into()),
    }
}

struct ProbeEffectInterpreter<'handler> {
    handler: &'handler MessageHandler,
}

impl<'handler> ProbeEffectInterpreter<'handler> {
    const fn new(handler: &'handler MessageHandler) -> Self {
        Self { handler }
    }

    async fn run(&self, effect: ProbeEffect<'_>) -> Result<()> {
        match effect {
            ProbeEffect::Forward { ctx } => {
                self.handler
                    .run_effects([CoreEffect::forward_payload(ctx, None)])
                    .await
            }
            ProbeEffect::RespondToRequest { ctx, request } => {
                self.respond_to_request(ctx, request).await
            }
            ProbeEffect::RespondToOffer { ctx, offer } => self.respond_to_offer(ctx, offer).await,
            ProbeEffect::AdmitAcknowledgement {
                ctx,
                acknowledgement,
            } => self.admit_acknowledgement(ctx, acknowledgement).await,
        }
    }

    async fn respond_to_request(&self, ctx: &MessagePayload, request: &ProbeRequest) -> Result<()> {
        let plan = plan_probe_offer(
            ctx,
            request,
            self.handler.dht.did,
            self.handler.transport.network_id,
            get_epoch_ms(),
        )?;
        let completion_sequence = *self
            .handler
            .transport
            .reserve_transaction_sequences(plan.beneficiary, NonZeroU64::MIN)
            .await?
            .start();
        let offer = plan.build(completion_sequence, self.handler.transport.message_signer())?;
        self.handler
            .run_effects([CoreEffect::send_report_message(
                ctx,
                Message::ProbeOffer(Box::new(offer)),
            )])
            .await
    }

    async fn respond_to_offer(&self, ctx: &MessagePayload, offer: &ProbeOffer) -> Result<()> {
        let beneficiary = self.handler.dht.did;
        let provider = ctx.transaction.origin();
        let request = offer.verify_live_transcript_at(
            ctx,
            self.handler.transport.network_id,
            beneficiary,
            get_epoch_ms(),
        )?;
        self.handler
            .transport
            .consume_pending_probe(provider, ctx.transaction.tx_id, request)?
            .then_some(())
            .ok_or_else(|| {
                Error::InvalidMessage("probe offer has no matching outstanding request".to_string())
            })?;
        let acknowledgement =
            build_probe_acknowledgement(offer, self.handler.transport.message_signer())?;
        self.handler
            .run_effects([CoreEffect::send_report_message(
                ctx,
                Message::ProbeAcknowledgement(Box::new(acknowledgement)),
            )])
            .await
    }

    async fn admit_acknowledgement(
        &self,
        ctx: &MessagePayload,
        acknowledgement: &ProbeAcknowledgement,
    ) -> Result<()> {
        let receipt = validate_acknowledgement_roles(
            acknowledgement,
            self.handler.dht.did,
            ctx.transaction.origin(),
        )?;
        self.handler
            .transport
            .admit_provisional_receipt(receipt, get_epoch_ms())
            .await
    }
}

/// Probe request is a direct authenticated overlay liveness probe.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ProbeRequest> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &ProbeRequest) -> Result<()> {
        ProbeEffectInterpreter::new(self)
            .run(ProbeEffect::request(self.dht.did, ctx, msg))
            .await
    }
}

/// Beneficiary verifies an offered transcript and returns its role attestation once.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ProbeOffer> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &ProbeOffer) -> Result<()> {
        ProbeEffectInterpreter::new(self)
            .run(ProbeEffect::offer(self.dht.did, ctx, msg))
            .await
    }
}

/// Provider verifies the acknowledgement before admitting bounded evidence.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ProbeAcknowledgement> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &ProbeAcknowledgement) -> Result<()> {
        ProbeEffectInterpreter::new(self)
            .run(ProbeEffect::acknowledgement(self.dht.did, ctx, msg))
            .await
    }
}
