use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use futures::channel::oneshot;

use super::OnionCircuitId;
use super::OnionHttpsClientResponse;
use super::OnionHttpsRuntime;
use crate::error::Error;

type PendingResponse = oneshot::Receiver<std::result::Result<OnionHttpsClientResponse, Error>>;

/// Response future and ownership guard for one pending HTTPS onion request.
///
/// Invariant: dropping the request future removes its circuit from the runtime,
/// including cancellation by the browser bridge.
pub(crate) struct PendingOnionHttpsRequest {
    runtime: Arc<OnionHttpsRuntime>,
    id: OnionCircuitId,
    response: PendingResponse,
}

impl PendingOnionHttpsRequest {
    pub(super) const fn new(
        runtime: Arc<OnionHttpsRuntime>,
        id: OnionCircuitId,
        response: PendingResponse,
    ) -> Self {
        Self {
            runtime,
            id,
            response,
        }
    }

    #[cfg(test)]
    pub(crate) fn try_recv(
        &mut self,
    ) -> std::result::Result<
        Option<std::result::Result<OnionHttpsClientResponse, Error>>,
        oneshot::Canceled,
    > {
        self.response.try_recv()
    }
}

impl Future for PendingOnionHttpsRequest {
    type Output = std::result::Result<
        std::result::Result<OnionHttpsClientResponse, Error>,
        oneshot::Canceled,
    >;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.response).poll(cx)
    }
}

impl Drop for PendingOnionHttpsRequest {
    fn drop(&mut self) {
        self.runtime.cancel_request(self.id);
    }
}
