//! Probe provisional service-receipt state-machine handlers.
//!
//! Pure planners construct transcript values. [`ProbeEffect`] names external work, and
//! [`ProbeEffectInterpreter`] is the only adapter that reads clocks or mutates transport state.

use std::borrow::Borrow;
use std::num::NonZeroU64;

use async_trait::async_trait;

use crate::delegation::DelegateeKey;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::effects::CoreEffect;
use crate::message::handlers::HandlerContext;
use crate::message::HandleMsg;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessageKind;
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

/// Classified handler effect for an inbound provisional probe transcript message.
enum ProbeEffect<'payload> {
    /// Reject a probe message that was relayed or addressed to another node.
    RejectNonDirect,
    /// Answer a direct [`ProbeRequest`] with a signed [`ProbeOffer`].
    RespondToRequest {
        /// Inbound payload carrying the probe request.
        ctx: &'payload MessagePayload,
        /// Decoded probe request body.
        request: &'payload ProbeRequest,
    },
    /// Verify a direct [`ProbeOffer`] and reply with a [`ProbeAcknowledgement`].
    RespondToOffer {
        /// Inbound payload carrying the probe offer.
        ctx: &'payload MessagePayload,
        /// Decoded probe offer body.
        offer: &'payload ProbeOffer,
    },
    /// Verify and admit a direct [`ProbeAcknowledgement`] receipt.
    AdmitAcknowledgement {
        /// Inbound payload carrying the probe acknowledgement.
        ctx: &'payload MessagePayload,
        /// Decoded probe acknowledgement body.
        acknowledgement: &'payload ProbeAcknowledgement,
    },
}

impl<'payload> ProbeEffect<'payload> {
    /// Require direct local delivery from the transaction origin, or return [`Self::RejectNonDirect`].
    fn direct_local_only(local: Did, ctx: &'payload MessagePayload, local_effect: Self) -> Self {
        if ctx.should_forward_from(local)
            || ctx.delegations().hop.delegator_did() != ctx.transaction.origin()
        {
            Self::RejectNonDirect
        } else {
            local_effect
        }
    }

    /// Classify an inbound [`ProbeRequest`] for local node `local`.
    fn request(local: Did, ctx: &'payload MessagePayload, request: &'payload ProbeRequest) -> Self {
        Self::direct_local_only(local, ctx, Self::RespondToRequest { ctx, request })
    }

    /// Classify an inbound [`ProbeOffer`] for local node `local`.
    fn offer(local: Did, ctx: &'payload MessagePayload, offer: &'payload ProbeOffer) -> Self {
        Self::direct_local_only(local, ctx, Self::RespondToOffer { ctx, offer })
    }

    /// Classify an inbound [`ProbeAcknowledgement`] for local node `local`.
    fn acknowledgement(
        local: Did,
        ctx: &'payload MessagePayload,
        acknowledgement: &'payload ProbeAcknowledgement,
    ) -> Self {
        Self::direct_local_only(local, ctx, Self::AdmitAcknowledgement {
            ctx,
            acknowledgement,
        })
    }
}

/// Validated inputs prepared to construct a signed [`ProbeOffer`] response.
struct ProbeOfferPlan {
    /// Original signed probe request transaction.
    request: Transaction,
    /// Local provider DID answering the probe.
    provider: Did,
    /// Remote beneficiary DID that sent the probe request.
    beneficiary: Did,
    /// Overlay network identifier for the claim.
    network_id: u32,
    /// Accepted provisional wall-clock epoch from the request.
    epoch: ProvisionalEpoch,
    /// Request nonce echoed into the completion and claim.
    nonce: [u8; 32],
    /// Canonical digest bytes of the request transaction.
    request_digest: [u8; 32],
}

impl ProbeOfferPlan {
    /// Sign the completion transaction and provider attestation to assemble a [`ProbeOffer`].
    fn build(
        self,
        completion_sequence: u64,
        signer: MessageSigner<impl Borrow<DelegateeKey>>,
        ts_ms: u128,
    ) -> Result<ProbeOffer> {
        let completion = Transaction::new_at(
            self.beneficiary,
            self.request.tx_id,
            completion_sequence,
            None,
            ProbeCompletion {
                request_digest: self.request_digest,
                nonce: self.nonce,
            },
            signer.by_ref(),
            ts_ms,
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
        let provider_attestation = claim.sign_provider_at(signer.by_ref(), ts_ms)?;
        Ok(ProbeOffer {
            request: self.request,
            completion,
            claim,
            provider_attestation,
        })
    }
}

/// Validate `request`'s provisional epoch at `observed_at_ms` and build a [`ProbeOfferPlan`].
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

/// Sign the beneficiary attestation for `offer` and assemble a [`ProbeAcknowledgement`].
fn build_probe_acknowledgement(
    offer: &ProbeOffer,
    signer: MessageSigner<impl Borrow<DelegateeKey>>,
    ts_ms: u128,
) -> Result<ProbeAcknowledgement> {
    let beneficiary_attestation = offer.claim.sign_beneficiary_at(signer, ts_ms)?;
    let receipt = ProvisionalServiceReceipt::new(
        offer.claim.clone(),
        offer.provider_attestation.clone(),
        beneficiary_attestation,
    )?;
    Ok(ProbeAcknowledgement { receipt })
}

/// Verify that `acknowledgement`'s receipt names `provider` and `beneficiary` in their expected roles.
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

/// Effect interpreter executing classified [`ProbeEffect`] actions against a [`MessageHandler`].
struct ProbeEffectInterpreter<'handler> {
    /// Message handler providing transport, DHT, and signing capabilities.
    handler: &'handler MessageHandler,
    /// Injected handler timestamp in milliseconds.
    now_ms: u128,
}

impl<'handler> ProbeEffectInterpreter<'handler> {
    /// Create an interpreter for `handler` at timestamp `now_ms`.
    const fn new(handler: &'handler MessageHandler, now_ms: u128) -> Self {
        Self { handler, now_ms }
    }

    /// Execute one classified [`ProbeEffect`].
    async fn run(&self, effect: ProbeEffect<'_>) -> Result<()> {
        match effect {
            ProbeEffect::RejectNonDirect => Err(Error::InvalidMessage(
                "probe transcript messages must be delivered directly without relaying".to_string(),
            )),
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

    /// Answer an inbound [`ProbeRequest`] by reserving a completion sequence and sending a [`ProbeOffer`].
    async fn respond_to_request(&self, ctx: &MessagePayload, request: &ProbeRequest) -> Result<()> {
        let plan = plan_probe_offer(
            ctx,
            request,
            self.handler.dht.did,
            self.handler.transport.network_id,
            self.now_ms,
        )?;
        let completion_sequence = *self
            .handler
            .transport
            // The completion is signed evidence carried inside the offer, never admitted on its
            // own; it takes its sequence from the stream of the offer that carries it.
            .reserve_transaction_sequences(
                plan.beneficiary,
                MessageKind::ProbeOffer.class(),
                NonZeroU64::MIN,
            )
            .await?
            .start();
        let signer = self.handler.transport.message_signer();
        let offer = plan.build(completion_sequence, signer.by_ref(), self.now_ms)?;
        // The answer takes its next hop's answer slot, not the shared capacity, so peers that
        // hold this node's control reserve cannot keep it from answering.
        self.handler
            .transport
            .probe_answer_egress()
            .send_report_message(ctx, Message::ProbeOffer(Box::new(offer)))
            .await
    }

    /// Verify an inbound [`ProbeOffer`], match it against an outstanding liveness probe, and reply with an acknowledgement.
    async fn respond_to_offer(&self, ctx: &MessagePayload, offer: &ProbeOffer) -> Result<()> {
        let beneficiary = self.handler.dht.did;
        let provider = ctx.transaction.origin();
        let request = offer.verify_live_transcript_at(
            ctx,
            self.handler.transport.network_id,
            beneficiary,
            self.now_ms,
        )?;
        let now_ms_i64 = i64::try_from(self.now_ms).unwrap_or(i64::MAX);
        self.handler
            .transport
            .answer_liveness_probe_at(provider, request, now_ms_i64)?
            .then_some(())
            .ok_or_else(|| {
                Error::InvalidMessage("probe offer has no matching outstanding request".to_string())
            })?;
        let signer = self.handler.transport.message_signer();
        let acknowledgement = build_probe_acknowledgement(offer, signer.by_ref(), self.now_ms)?;
        self.handler
            .run_effects([CoreEffect::send_report_message(
                ctx,
                Message::ProbeAcknowledgement(Box::new(acknowledgement)),
            )])
            .await
    }

    /// Validate an inbound [`ProbeAcknowledgement`] and admit its provisional receipt into transport state.
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
            .admit_provisional_receipt(receipt, self.now_ms)
            .await
    }
}

/// Probe request is a direct authenticated overlay liveness probe.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ProbeRequest> for MessageHandler {
    async fn handle(&self, context: &HandlerContext<'_>, msg: &ProbeRequest) -> Result<()> {
        let ctx = context.payload();
        ProbeEffectInterpreter::new(self, context.now_ms())
            .run(ProbeEffect::request(self.dht.did, ctx, msg))
            .await
    }
}

/// Beneficiary verifies an offered transcript and returns its role attestation once.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ProbeOffer> for MessageHandler {
    async fn handle(&self, context: &HandlerContext<'_>, msg: &ProbeOffer) -> Result<()> {
        let ctx = context.payload();
        ProbeEffectInterpreter::new(self, context.now_ms())
            .run(ProbeEffect::offer(self.dht.did, ctx, msg))
            .await
    }
}

/// Provider verifies the acknowledgement before admitting bounded evidence.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ProbeAcknowledgement> for MessageHandler {
    async fn handle(&self, context: &HandlerContext<'_>, msg: &ProbeAcknowledgement) -> Result<()> {
        let ctx = context.payload();
        ProbeEffectInterpreter::new(self, context.now_ms())
            .run(ProbeEffect::acknowledgement(self.dht.did, ctx, msg))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::DEFAULT_DELEGATION_TTL_MS;
    use crate::dht::delivery::NextHop;
    use crate::dht::delivery::RouteStage;
    use crate::ecc::SecretKey;
    use crate::message::MessageRelay;
    use crate::tests::TEST_NETWORK_ID;

    #[test]
    fn probe_transcript_signing_uses_injected_clock_not_ambient_wall_clock() -> Result<()> {
        // Year 2033 ms — deliberately disjoint from host wall clock.
        let shifted_now_ms: u128 = 2_000_000_000_000;
        let beneficiary_key = SecretKey::random();
        let provider_key = SecretKey::random();
        let beneficiary_did: Did = beneficiary_key.address().into();
        let provider_did: Did = provider_key.address().into();

        let beneficiary_del = DelegateeKey::new_with_seckey_at(
            &beneficiary_key,
            DEFAULT_DELEGATION_TTL_MS,
            shifted_now_ms,
        )?;
        let provider_del = DelegateeKey::new_with_seckey_at(
            &provider_key,
            DEFAULT_DELEGATION_TTL_MS,
            shifted_now_ms,
        )?;
        let beneficiary_signer = MessageSigner::new(&beneficiary_del, TEST_NETWORK_ID);
        let provider_signer = MessageSigner::new(&provider_del, TEST_NETWORK_ID);

        let request = ProbeRequest::random_for_epoch(ProvisionalEpoch::from_unix_seconds(
            u64::try_from(shifted_now_ms / 1_000).unwrap_or(u64::MAX),
        ));
        let tx = Transaction::new_at(
            provider_did,
            uuid::Uuid::new_v4(),
            1,
            None,
            Message::ProbeRequest(request),
            beneficiary_signer.by_ref(),
            shifted_now_ms,
        )?;
        let req_payload = MessagePayload::new(
            tx,
            beneficiary_signer.by_ref(),
            MessageRelay::new(
                NextHop::new(provider_did, RouteStage::TOWARD),
                provider_did,
                crate::message::HopBudget::MAX,
            ),
        )?;

        let plan = plan_probe_offer(
            &req_payload,
            &request,
            provider_did,
            TEST_NETWORK_ID,
            shifted_now_ms,
        )?;
        let offer = plan.build(7, provider_signer.by_ref(), shifted_now_ms)?;
        assert_eq!(offer.completion.verification.ts_ms, shifted_now_ms);
        assert_eq!(offer.provider_attestation.ts_ms, shifted_now_ms);

        let offer_tx = Transaction::new_at(
            beneficiary_did,
            req_payload.transaction.tx_id,
            1,
            None,
            Message::ProbeOffer(Box::new(offer.clone())),
            provider_signer.by_ref(),
            shifted_now_ms,
        )?;
        let offer_payload = MessagePayload::new(
            offer_tx,
            provider_signer,
            MessageRelay::new(
                NextHop::new(beneficiary_did, RouteStage::replying_via(None)),
                beneficiary_did,
                crate::message::HopBudget::MAX,
            ),
        )?;
        let verified_req = offer.verify_live_transcript_at(
            &offer_payload,
            TEST_NETWORK_ID,
            beneficiary_did,
            shifted_now_ms,
        )?;
        assert_eq!(verified_req, request);

        let ack = build_probe_acknowledgement(&offer, beneficiary_signer, shifted_now_ms)?;
        assert_eq!(ack.receipt.beneficiary_attestation.ts_ms, shifted_now_ms);
        assert!(ack
            .receipt
            .verify_live_at(TEST_NETWORK_ID, shifted_now_ms)
            .is_ok());
        assert!(matches!(
            ack.receipt
                .verify_live_at(TEST_NETWORK_ID, crate::utils::get_epoch_ms()),
            Err(ServiceReceiptError::ProviderAttestationNotLive)
        ));
        Ok(())
    }
}
