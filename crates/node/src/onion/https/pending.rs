use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use futures::channel::oneshot;
use futures::future::Either;

use super::client::OnionHttpsClient;
use super::client::OnionHttpsOutcome;
use super::OnionCircuitId;
use crate::error::Error;
use crate::error::Result;

type PendingResponse = oneshot::Receiver<OnionHttpsOutcome>;

/// Response future and ownership guard for one pending HTTPS onion request.
///
/// Invariant: dropping the request future removes its circuit from the client, whether the drop is
/// caller cancellation, a fired deadline or a failed send.
pub(crate) struct PendingOnionHttpsRequest {
    client: Arc<OnionHttpsClient>,
    id: OnionCircuitId,
    response: PendingResponse,
}

impl PendingOnionHttpsRequest {
    pub(super) const fn new(
        client: Arc<OnionHttpsClient>,
        id: OnionCircuitId,
        response: PendingResponse,
    ) -> Self {
        Self {
            client,
            id,
            response,
        }
    }

    /// Wait for the terminal outcome unless `deadline` resolves first.
    ///
    /// `deadline` is the platform timer effect: `Ok(())` means the wait expired and yields
    /// [`Error::OnionProxyRequestTimedOut`]; a timer failure is returned unchanged. Either way
    /// `self` is consumed, so the circuit has left the pending table when this returns.
    pub(crate) async fn within(
        self,
        deadline: impl Future<Output = Result<()>>,
    ) -> OnionHttpsOutcome {
        futures::pin_mut!(deadline);
        match futures::future::select(self, deadline).await {
            Either::Left((Ok(outcome), _)) => outcome,
            Either::Left((Err(oneshot::Canceled), _)) => Err(Error::HttpRequestError(
                "onion HTTPS proxy response channel closed".to_string(),
            )),
            Either::Right((Ok(()), _)) => Err(Error::OnionProxyRequestTimedOut),
            Either::Right((Err(error), _)) => Err(error),
        }
    }

    /// Circuit owned by this guard.
    #[cfg(test)]
    pub(crate) const fn circuit_id(&self) -> OnionCircuitId {
        self.id
    }
}

impl Future for PendingOnionHttpsRequest {
    type Output = std::result::Result<OnionHttpsOutcome, oneshot::Canceled>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.response).poll(cx)
    }
}

impl Drop for PendingOnionHttpsRequest {
    fn drop(&mut self) {
        self.client.cancel(self.id);
    }
}
