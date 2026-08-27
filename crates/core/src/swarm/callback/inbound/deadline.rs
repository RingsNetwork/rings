use std::future::Future;
use std::time::Duration;

use futures::FutureExt;

use crate::utils::sleep;

pub(super) enum InboundDeadline<T> {
    Completed(T),
    TimedOut,
}

pub(super) async fn await_inbound_deadline<F, T>(
    future: F,
    timeout: Duration,
) -> InboundDeadline<T>
where
    F: Future<Output = T>,
{
    let future = future.fuse();
    let deadline = sleep(timeout).fuse();
    futures::pin_mut!(future, deadline);
    futures::select! {
        output = future => InboundDeadline::Completed(output),
        () = deadline => InboundDeadline::TimedOut,
    }
}
