//! Ownership-boundary regressions for the single native retirement authority.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::send_lifecycle::OwnedSend;
use super::send_lifecycle::SendLifecycle;
use super::send_runtime::native_send_runtime;
use super::send_runtime::run_irrevocable_send_with_timeout;
use super::send_runtime::run_send_with_retirement;
use super::send_runtime::NativeRetirementFence;
use crate::core::transport::ConnectionStateCell;
use crate::core::transport::SendPermit;
use crate::error::Error;
use crate::error::Result;

/// Captured resource that witnesses fencing at the owner-destruction boundary.
struct PendingResource {
    /// Logical-close signal set synchronously by the authority.
    fenced: CancellationToken,
    /// Result of checking the signal when the resource is released.
    fenced_at_drop: Arc<AtomicBool>,
    /// Inject a poll panic instead of remaining pending.
    panic_on_poll: bool,
}

impl Future for PendingResource {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.panic_on_poll, "injected primitive panic");
        Poll::Pending
    }
}

impl Drop for PendingResource {
    fn drop(&mut self) {
        self.fenced_at_drop
            .store(self.fenced.is_cancelled(), Ordering::Release);
    }
}

/// Wait for cleanup in a bounded test, never interpreting termination as physical success.
async fn cleanup(lifecycle: &SendLifecycle) {
    tokio::time::timeout(Duration::from_secs(1), lifecycle.wait_for_cleanup())
        .await
        .expect("cleanup must terminate");
}

/// Exercise every admission phase with several failure-observer multiplicities.
#[tokio::test]
async fn only_irrevocable_failures_consume_the_close_capability() {
    for phase in 0..4 {
        for observations in 1..=4 {
            let permit = SendPermit::always();
            let acceptance = permit.acceptance();
            let fenced = CancellationToken::new();
            let closes = Arc::new(AtomicUsize::new(0));
            let close_count = Arc::clone(&closes);
            let lifecycle = SendLifecycle::new(
                native_send_runtime().expect("test runtime"),
                acceptance.clone(),
                NativeRetirementFence::new(ConnectionStateCell::new(), fenced.clone()),
                async move {
                    close_count.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                },
            );
            match phase {
                0 => drop(permit),
                1 => {
                    assert!(acceptance.try_cancel());
                    drop(permit);
                }
                2 => drop(permit.try_mark_irrevocable().expect("claim")),
                3 => permit
                    .try_mark_irrevocable()
                    .expect("claim")
                    .mark_accepted(),
                _ => unreachable!(),
            }
            for _ in 0..observations {
                lifecycle.fail();
            }
            cleanup(&lifecycle).await;
            assert_eq!(fenced.is_cancelled(), phase == 2);
            assert_eq!(closes.load(Ordering::Acquire), usize::from(phase == 2));
        }
    }
}

/// Concurrent caller/worker failures race for one close capability, not two guards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_failures_start_exactly_one_physical_close() {
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let _proof = permit.try_mark_irrevocable().expect("claim");
    let fenced = CancellationToken::new();
    let closes = Arc::new(AtomicUsize::new(0));
    let close_count = Arc::clone(&closes);
    let lifecycle = SendLifecycle::new(
        native_send_runtime().expect("test runtime"),
        acceptance,
        NativeRetirementFence::new(ConnectionStateCell::new(), fenced.clone()),
        async move {
            close_count.fetch_add(1, Ordering::AcqRel);
            Ok(())
        },
    );
    let start = Arc::new(std::sync::Barrier::new(3));
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let start = Arc::clone(&start);
            let lifecycle = Arc::clone(&lifecycle);
            scope.spawn(move || {
                start.wait();
                lifecycle.fail();
            });
        }
        start.wait();
    });
    cleanup(&lifecycle).await;
    assert!(fenced.is_cancelled());
    assert_eq!(closes.load(Ordering::Acquire), 1);
}

/// Later acceptance cannot revoke an already committed retirement decision.
#[tokio::test]
async fn late_acceptance_does_not_cancel_in_flight_cleanup() {
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let proof = permit.try_mark_irrevocable().expect("claim");
    let fenced = CancellationToken::new();
    let started = CancellationToken::new();
    let release = CancellationToken::new();
    let close_started = started.clone();
    let close_release = release.clone();
    let lifecycle = SendLifecycle::new(
        native_send_runtime().expect("test runtime"),
        acceptance,
        NativeRetirementFence::new(ConnectionStateCell::new(), fenced.clone()),
        async move {
            close_started.cancel();
            close_release.cancelled().await;
            Ok(())
        },
    );
    lifecycle.fail();
    started.cancelled().await;
    proof.mark_accepted();
    lifecycle.fail();
    assert!(fenced.is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_millis(10), lifecycle.wait_for_cleanup())
            .await
            .is_err()
    );
    release.cancel();
    cleanup(&lifecycle).await;
}

/// Both timeout and panic must fence before releasing the primitive's captures.
#[tokio::test]
async fn timeout_and_panic_fence_before_owned_resources_drop() {
    for panic_on_poll in [false, true] {
        let permit = SendPermit::always();
        let acceptance = permit.acceptance();
        let _proof = permit.try_mark_irrevocable().expect("claim");
        let fenced = CancellationToken::new();
        let fenced_at_drop = Arc::new(AtomicBool::new(false));
        let lifecycle = SendLifecycle::new(
            native_send_runtime().expect("test runtime"),
            acceptance,
            NativeRetirementFence::new(ConnectionStateCell::new(), fenced.clone()),
            async { Ok(()) },
        );
        let send = OwnedSend::new(
            PendingResource {
                fenced,
                fenced_at_drop: Arc::clone(&fenced_at_drop),
                panic_on_poll,
            },
            Arc::clone(&lifecycle),
        );
        let result = run_irrevocable_send_with_timeout(
            &native_send_runtime().expect("test runtime"),
            Duration::from_millis(10),
            send,
        )
        .await;
        if panic_on_poll {
            assert!(matches!(result, Err(Error::NativeSendTask(error)) if error.is_panic()));
        } else {
            assert!(matches!(
                result,
                Err(Error::NativeSendCompletionTimeout { .. })
            ));
        }
        cleanup(&lifecycle).await;
        assert!(fenced_at_drop.load(Ordering::Acquire));
    }
}

/// Cancelling the error waiter cannot cancel the unique physical close operation.
#[tokio::test]
async fn close_survives_cancellation_while_reporting_send_failure() {
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let fenced = CancellationToken::new();
    let started = CancellationToken::new();
    let release = CancellationToken::new();
    let finished = CancellationToken::new();
    let close_started = started.clone();
    let close_release = release.clone();
    let close_finished = finished.clone();
    let fence = NativeRetirementFence::new(ConnectionStateCell::new(), fenced.clone());
    let caller = tokio::spawn(async move {
        run_send_with_retirement(
            &native_send_runtime().expect("test runtime"),
            acceptance,
            fence,
            |_| async move {
                let _proof = permit.try_mark_irrevocable().expect("claim");
                Err::<(), _>(Error::NativeSendCompletionTimeout { timeout_ms: 7 })
            },
            async move {
                close_started.cancel();
                close_release.cancelled().await;
                close_finished.cancel();
                Ok(())
            },
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(1), started.cancelled())
        .await
        .expect("close starts");
    assert!(fenced.is_cancelled());
    caller.abort();
    let _cancelled = caller.await;
    release.cancel();
    tokio::time::timeout(Duration::from_secs(1), finished.cancelled())
        .await
        .expect("close survives");
}

/// The executor may discard an unpolled continuation during shutdown.
#[test]
fn runtime_shutdown_fences_an_unpolled_owner_without_claiming_physical_success() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let _proof = permit.try_mark_irrevocable().expect("claim");
    let fenced = CancellationToken::new();
    let fenced_at_drop = Arc::new(AtomicBool::new(false));
    let physical_success = Arc::new(AtomicBool::new(false));
    let closed = Arc::clone(&physical_success);
    let lifecycle = SendLifecycle::new(
        runtime.handle().clone(),
        acceptance,
        NativeRetirementFence::new(ConnectionStateCell::new(), fenced.clone()),
        async move {
            closed.store(true, Ordering::Release);
            Ok(())
        },
    );
    let send = OwnedSend::new(
        PendingResource {
            fenced: fenced.clone(),
            fenced_at_drop: Arc::clone(&fenced_at_drop),
            panic_on_poll: false,
        },
        Arc::clone(&lifecycle),
    );
    runtime.spawn(send);
    drop(runtime);
    assert!(fenced.is_cancelled());
    assert!(fenced_at_drop.load(Ordering::Acquire));
    assert!(!physical_success.load(Ordering::Acquire));
    futures::executor::block_on(lifecycle.wait_for_cleanup());
}

/// The real queue-operation owner retains its channel lease through error and panic.
#[tokio::test]
async fn queue_owner_fences_before_releasing_the_channel_lease() {
    for panic_on_poll in [false, true] {
        let channel = Arc::new(tokio::sync::Mutex::new(()));
        let lease = Arc::clone(&channel).lock_owned().await;
        let permit = SendPermit::always();
        let acceptance = permit.acceptance();
        let fenced = CancellationToken::new();
        let fence = NativeRetirementFence::new(ConnectionStateCell::new(), fenced.clone());
        let lifecycle = SendLifecycle::new(
            native_send_runtime().expect("runtime"),
            acceptance,
            fence.clone(),
            async { Ok(()) },
        );
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(10));
        let primitive = async move {
            assert!(!panic_on_poll, "injected primitive panic");
            Err(Error::NativeSendCompletionTimeout { timeout_ms: 1 })
        };
        let queue = super::send_operation::QueueSend::new(
            primitive,
            permit,
            lease,
            Arc::clone(&counter),
            5,
        )
        .expect("offset fits");
        let mut owner = OwnedSend::new(queue, Arc::clone(&lifecycle));
        let admission = fence.try_send_admission().expect("open fence");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner.poll_admitted(admission)
        }));
        assert!(fenced.is_cancelled());
        assert!(
            channel.try_lock().is_err(),
            "lease is still owned after failure was fenced"
        );
        assert_eq!(
            counter.load(Ordering::Acquire),
            10,
            "failed bytes are not committed"
        );
        assert_eq!(outcome.is_err(), panic_on_poll);
        drop(owner);
        assert!(channel.try_lock().is_ok());
        cleanup(&lifecycle).await;
    }
}

/// Successful queue admission publishes its exact offset without requesting close.
#[tokio::test]
async fn queue_acceptance_commits_bytes_once_and_retains_a_usable_generation() {
    let channel = Arc::new(tokio::sync::Mutex::new(()));
    let lease = Arc::clone(&channel).lock_owned().await;
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let fenced = CancellationToken::new();
    let fence = NativeRetirementFence::new(ConnectionStateCell::new(), fenced.clone());
    let lifecycle = SendLifecycle::new(
        native_send_runtime().expect("runtime"),
        acceptance.clone(),
        fence.clone(),
        async { Ok(()) },
    );
    let counter = Arc::new(std::sync::atomic::AtomicU64::new(10));
    let queue = super::send_operation::QueueSend::new(
        async { Ok(()) },
        permit,
        lease,
        Arc::clone(&counter),
        5,
    )
    .expect("offset fits");
    let mut owner = OwnedSend::new(queue, lifecycle);
    let admission = fence.try_send_admission().expect("open fence");
    assert!(matches!(
        owner.poll_admitted(admission),
        Poll::Ready(Ok(15))
    ));
    assert!(acceptance.is_accepted());
    assert_eq!(counter.load(Ordering::Acquire), 15);
    drop(owner);
    assert!(!fenced.is_cancelled());
    assert!(channel.try_lock().is_ok());
}

/// Counter exhaustion is rejected before the operation can consume its permit.
#[tokio::test]
async fn exhausted_byte_accounting_cannot_start_a_physical_write() {
    let channel = Arc::new(tokio::sync::Mutex::new(()));
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let attempted = Arc::new(AtomicBool::new(false));
    let writing = Arc::clone(&attempted);
    let queue = super::send_operation::QueueSend::new(
        async move {
            writing.store(true, Ordering::Release);
            Ok(())
        },
        permit,
        channel.lock_owned().await,
        Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX)),
        1,
    );
    assert!(matches!(queue, Err(Error::SendByteCountOverflow)));
    assert!(!attempted.load(Ordering::Acquire));
    assert!(acceptance.try_cancel());
}

/// Caller cancellation fences immediately while the detached operation retains its lease.
#[tokio::test]
async fn caller_and_detached_failure_share_one_close_and_preserve_handoff_ownership() {
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let fenced = CancellationToken::new();
    let fence = NativeRetirementFence::new(ConnectionStateCell::new(), fenced.clone());
    let channel = Arc::new(tokio::sync::Mutex::new(()));
    let sending_channel = Arc::clone(&channel);
    let started = CancellationToken::new();
    let sending_started = started.clone();
    let release = CancellationToken::new();
    let sending_release = release.clone();
    let finished = CancellationToken::new();
    let sending_finished = finished.clone();
    let closes = Arc::new(AtomicUsize::new(0));
    let close_count = Arc::clone(&closes);
    let caller = tokio::spawn(async move {
        let runtime = native_send_runtime().expect("runtime");
        run_send_with_retirement(
            &runtime,
            acceptance,
            fence.clone(),
            move |lifecycle| async move {
                let primitive = async move {
                    sending_started.cancel();
                    sending_release.cancelled().await;
                    Err(Error::NativeSendCompletionTimeout { timeout_ms: 3 })
                };
                let queue = super::send_operation::QueueSend::new(
                    primitive,
                    permit,
                    sending_channel.lock_owned().await,
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    5,
                )?;
                let mut owner = OwnedSend::new(queue, lifecycle);
                let admission = fence.try_send_admission().expect("open fence");
                assert!(owner.poll_admitted(admission).is_pending());
                // This completion signal belongs to the continuation itself, not
                // the cancelled caller waiting on its JoinHandle.
                let continuation = async move {
                    let result = owner.await;
                    sending_finished.cancel();
                    result
                };
                // Tokio task ownership outlives cancellation of its join waiter.
                native_send_runtime()
                    .expect("runtime")
                    .spawn(continuation)
                    .await
                    .map_err(Error::NativeSendTask)?
            },
            async move {
                close_count.fetch_add(1, Ordering::AcqRel);
                Ok(())
            },
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(1), started.cancelled())
        .await
        .expect("send starts");
    caller.abort();
    let _cancelled = caller.await;
    assert!(
        fenced.is_cancelled(),
        "caller cancellation fences synchronously"
    );
    assert!(
        channel.try_lock().is_err(),
        "background operation still owns the lease"
    );
    release.cancel();
    tokio::time::timeout(Duration::from_secs(1), finished.cancelled())
        .await
        .expect("background send finishes");
    // Join scheduling may publish the primitive result before its owner is dropped.
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if channel.try_lock().is_ok() && closes.load(Ordering::Acquire) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("lease released and exactly one close");
    assert_eq!(closes.load(Ordering::Acquire), 1);
}
