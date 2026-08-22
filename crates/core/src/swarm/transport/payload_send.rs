use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;

use super::delivery::ChunkSendPermit;
use super::delivery::SendCompletionOutcome;
use super::outbound::ChunkFrames;
use super::outbound::OutboundCompletion;
use super::outbound::OutboundMessageMeta;
use super::outbound::OutboundTransfer;
use super::outbound::OutboundTransferRoute;
use super::AdmittedConnection;
use super::SwarmTransport;
use crate::chunk::ChunkList;
use crate::chunk::Framing;
use crate::chunk::WireReserves;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::session::SessionSk;
use crate::utils::sleep;

#[cfg(test)]
const TRACKED_PAYLOAD_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(not(test))]
const TRACKED_PAYLOAD_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Clone, Copy)]
enum SendCompletion {
    Detached,
    Tracked,
}

impl From<SendCompletion> for OutboundCompletion {
    fn from(value: SendCompletion) -> Self {
        match value {
            SendCompletion::Detached => Self::Detached,
            SendCompletion::Tracked => Self::Tracked,
        }
    }
}

struct OversizedPayloadLog {
    local: Did,
    next_hop: Did,
    destination: Did,
    relay_destination: Did,
    tx_id: String,
    message_kind: &'static str,
    bytes: usize,
    max_bytes: usize,
}

fn log_oversized_payload(metadata: OversizedPayloadLog) {
    tracing::error!(
        local = %metadata.local,
        next_hop = %metadata.next_hop,
        destination = %metadata.destination,
        relay_destination = %metadata.relay_destination,
        tx_id = %metadata.tx_id,
        message_kind = metadata.message_kind,
        bytes = metadata.bytes,
        max_bytes = metadata.max_bytes,
        "message payload is too large"
    );
}

impl SwarmTransport {
    /// Send a maintenance payload and return only after all of its frames stop.
    ///
    /// This is a network-only cancellation boundary. Dropping the send future
    /// cannot cancel storage work, and bounds a delivery future that never
    /// observes buffered-amount recovery.
    pub(crate) async fn send_payload_tracked(
        &self,
        payload: MessagePayload,
    ) -> Result<SendCompletionOutcome> {
        let did = payload.relay.next_hop;
        let send = self
            .do_send_payload_with_completion(
                payload.relay.next_hop,
                payload,
                SendCompletion::Tracked,
            )
            .fuse();
        let timeout = sleep(TRACKED_PAYLOAD_TIMEOUT).fuse();
        pin_mut!(send, timeout);

        select! {
            result = send => result,
            _ = timeout => {
                tracing::warn!(
                    target: "rings_core::transport::tracked_send",
                    local = %self.dht.did,
                    peer = %did,
                    timeout_ms = TRACKED_PAYLOAD_TIMEOUT.as_millis(),
                    "tracked payload delivery timed out and was deferred"
                );
                Ok(SendCompletionOutcome::Cancelled)
            }
        }
    }

    async fn connection_for_send(
        &self,
        did: Did,
        completion: SendCompletion,
        records_missing_connection_failure: bool,
    ) -> Result<AdmittedConnection> {
        match completion {
            SendCompletion::Detached => {
                let Some(connection) = self.get_and_check_send_connection(did).await else {
                    if records_missing_connection_failure {
                        self.record_peer_message_send_failed(did).await;
                    }
                    return Err(Error::SwarmMissDidInTable(did));
                };
                Ok(connection)
            }
            SendCompletion::Tracked => {
                let Some(connection) = self.admitted_send_connection(did)? else {
                    return Err(Error::SwarmMissDidInTable(did));
                };
                connection
                    .connection()
                    .readiness()
                    .ensure_can_make_progress()?;
                Ok(connection)
            }
        }
    }

    async fn submit_outbound_transfer(
        &self,
        admitted: &AdmittedConnection,
        did: Did,
        transfer: OutboundTransfer,
        receiver: futures::channel::oneshot::Receiver<Result<SendCompletionOutcome>>,
    ) -> Result<SendCompletionOutcome> {
        let Some(handle) = admitted.with_current(|_| self.outbound_schedulers.handle(did))? else {
            return Ok(SendCompletionOutcome::Cancelled);
        };
        if let Err(error) = handle.submit(transfer).await {
            return match admitted.ensure_current() {
                Ok(()) => Err(error),
                Err(Error::ConnectionAttemptSuperseded { .. }) => {
                    Ok(SendCompletionOutcome::Cancelled)
                }
                Err(current_error) => Err(current_error),
            };
        }
        match receiver.await {
            Ok(result) => result,
            Err(_) => match admitted.ensure_current() {
                Ok(()) => Err(Error::ChannelRecvMessageFailed(
                    "outbound scheduler stopped".into(),
                )),
                Err(Error::ConnectionAttemptSuperseded { .. }) => {
                    Ok(SendCompletionOutcome::Cancelled)
                }
                Err(error) => Err(error),
            },
        }
    }

    async fn do_send_payload_with_completion(
        &self,
        did: Did,
        payload: MessagePayload,
        completion: SendCompletion,
    ) -> Result<SendCompletionOutcome> {
        self.do_send_payload_with_completion_observing(did, payload, completion, || {})
            .await
    }

    async fn do_send_payload_with_completion_observing(
        &self,
        did: Did,
        payload: MessagePayload,
        completion: SendCompletion,
        observe_before_scheduler_submit: impl FnOnce(),
    ) -> Result<SendCompletionOutcome> {
        let (permit, message_metadata) = {
            let message = payload.transaction.data::<Message>()?;
            (
                ChunkSendPermit::for_message(self.dht.clone(), did, &message),
                OutboundMessageMeta::from_message(&message),
            )
        };
        let message_kind = message_metadata.kind();
        let admitted = self
            .connection_for_send(did, completion, permit.records_missing_connection_failure())
            .await?;
        let tx_id = payload.transaction.tx_id;
        let destination = payload.transaction.destination;
        let relay_destination = payload.relay.destination;
        let next_hop = payload.relay.next_hop;
        let data = payload.to_wire()?;
        if data.len() > TRANSPORT_MAX_SIZE {
            log_oversized_payload(OversizedPayloadLog {
                local: self.dht.did,
                next_hop,
                destination,
                relay_destination,
                tx_id: tx_id.to_string(),
                message_kind,
                bytes: data.len(),
                max_bytes: TRANSPORT_MAX_SIZE,
            });
            return Err(Error::MessageTooLarge(data.len()));
        }

        let max_message_size = admitted.connection().max_message_size();
        let Some(plan) = WireReserves::PRODUCTION.plan(data.len(), max_message_size) else {
            self.record_peer_message_send_failed(did).await;
            return Err(Error::PeerMaxMessageSizeTooSmall(max_message_size));
        };
        tracing::debug!(
            local = %self.dht.did,
            next_hop = %next_hop,
            destination = %destination,
            relay_destination = %relay_destination,
            tx_id = %tx_id,
            message_kind,
            bytes = data.len(),
            max_message_size,
            framing = ?plan,
            "send payload start"
        );
        let completion_policy = OutboundCompletion::from(completion);
        let (transfer, receiver) = match plan {
            Framing::Whole => OutboundTransfer::whole(
                OutboundTransferRoute::new(
                    message_metadata.class(),
                    did,
                    admitted.clone(),
                    permit,
                    self.measure.clone(),
                ),
                data,
                completion_policy,
            ),
            Framing::Chunked { chunk_size } => {
                let chunks: ChunkFrames = Box::new(ChunkList::stream(data, chunk_size));
                OutboundTransfer::chunked(
                    OutboundTransferRoute::new(
                        message_metadata.class(),
                        did,
                        admitted.clone(),
                        permit,
                        self.measure.clone(),
                    ),
                    self.session_sk.clone(),
                    chunks,
                    completion_policy,
                )
            }
        };
        observe_before_scheduler_submit();
        let outcome = self
            .submit_outbound_transfer(&admitted, did, transfer, receiver)
            .await?;

        tracing::debug!(
            local = %self.dht.did,
            next_hop = %next_hop,
            destination = %destination,
            relay_destination = %relay_destination,
            tx_id = %tx_id,
            message_kind,
            tracked = matches!(completion, SendCompletion::Tracked),
            succeeded = matches!(outcome, SendCompletionOutcome::Succeeded),
            "send payload accepted"
        );
        Ok(outcome)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn send_payload_detached_observing_scheduler_submit_for_test(
        &self,
        payload: MessagePayload,
        observe_before_scheduler_submit: impl FnOnce(),
    ) -> Result<SendCompletionOutcome> {
        self.do_send_payload_with_completion_observing(
            payload.relay.next_hop,
            payload,
            SendCompletion::Detached,
            observe_before_scheduler_submit,
        )
        .await
    }

    pub(super) async fn send_payload_detached_with_outcome(
        &self,
        payload: MessagePayload,
    ) -> Result<SendCompletionOutcome> {
        self.do_send_payload_with_completion(
            payload.relay.next_hop,
            payload,
            SendCompletion::Detached,
        )
        .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl PayloadSender for SwarmTransport {
    fn session_sk(&self) -> &SessionSk {
        &self.session_sk
    }

    fn dht(&self) -> Arc<PeerRing> {
        self.dht.clone()
    }

    fn is_connected(&self, did: Did) -> bool {
        self.get_connection(did).is_some()
    }

    async fn do_send_payload(&self, did: Did, payload: MessagePayload) -> Result<()> {
        self.do_send_payload_with_completion(did, payload, SendCompletion::Detached)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use tracing_test::traced_test;

    use super::*;

    #[test]
    #[traced_test]
    fn oversized_payload_log_omits_message_body() {
        let secret_body = "do-not-log-this-custom-payload-body";
        log_oversized_payload(OversizedPayloadLog {
            local: Did::from(1_u32),
            next_hop: Did::from(2_u32),
            destination: Did::from(3_u32),
            relay_destination: Did::from(4_u32),
            tx_id: "tx-oversized-669".to_string(),
            message_kind: "CustomMessage",
            bytes: TRANSPORT_MAX_SIZE.saturating_add(1),
            max_bytes: TRANSPORT_MAX_SIZE,
        });

        assert!(logs_contain("message payload is too large"));
        assert!(logs_contain("tx-oversized-669"));
        assert!(logs_contain("CustomMessage"));
        assert!(logs_contain(
            &(TRANSPORT_MAX_SIZE.saturating_add(1)).to_string()
        ));
        assert!(logs_contain(&TRANSPORT_MAX_SIZE.to_string()));
        assert!(!logs_contain(secret_body));
        assert!(!logs_contain("CustomMessage {"));
    }
}
