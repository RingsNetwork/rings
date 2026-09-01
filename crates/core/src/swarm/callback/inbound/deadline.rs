use std::future::Future;
use std::time::Duration;

use futures::FutureExt;

use crate::utils::try_sleep;

pub(super) enum InboundDeadline<T> {
    Completed(T),
    TimedOut,
    TimerUnavailable,
}

pub(super) async fn await_inbound_deadline<F, T>(
    future: F,
    timeout: Duration,
) -> InboundDeadline<T>
where
    F: Future<Output = T>,
{
    await_inbound_deadline_with_timer(future, try_sleep(timeout)).await
}

async fn await_inbound_deadline_with_timer<F, T, D>(future: F, deadline: D) -> InboundDeadline<T>
where
    F: Future<Output = T>,
    D: Future<Output = bool>,
{
    let future = future.fuse();
    let deadline = deadline.fuse();
    futures::pin_mut!(future, deadline);
    futures::select! {
        output = future => InboundDeadline::Completed(output),
        completed = deadline => if completed {
            InboundDeadline::TimedOut
        } else {
            InboundDeadline::TimerUnavailable
        },
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejected_timer_fails_closed_without_polling_forever() {
        let outcome = await_inbound_deadline_with_timer(
            futures::future::pending::<()>(),
            futures::future::ready(false),
        )
        .await;
        assert!(matches!(outcome, InboundDeadline::TimerUnavailable));
    }
}
