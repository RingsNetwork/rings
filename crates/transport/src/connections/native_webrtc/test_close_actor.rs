//! Conformance witnesses between the production actor shell and its pure reducer.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::close_actor;
use super::send_lifecycle::SendLifecycle;
use super::send_model::close_step;
use super::send_model::CloseEvent;
use super::send_model::CloseOutcome;
use super::send_model::CloseState;
use super::send_runtime::native_send_runtime;
use super::send_runtime::NativeRetirementFence;
use crate::core::transport::ConnectionStateCell;
use crate::core::transport::SendPermit;
use crate::error::Error;

/// Supply the actual generation gate; tests cannot forge actor messages.
fn fence() -> NativeRetirementFence {
    NativeRetirementFence::new(ConnectionStateCell::new(), CancellationToken::new())
}

/// Actor commands start one close and expose the same terminal states as close_step.
#[tokio::test]
async fn actor_success_and_failure_conform_to_reducer_traces() {
    for succeeds in [true, false] {
        // The actor waits until the test has inspected its intermediate Closing state.
        let release = CancellationToken::new();
        let close_release = release.clone();
        // Count actual side effects, independently of the reducer's PublishClosingAndStart effect.
        let count = Arc::new(AtomicUsize::new(0));
        let close_count = Arc::clone(&count);
        let runtime = native_send_runtime().expect("test runtime");
        let (mailbox, mut status) = close_actor::spawn(&runtime, async move {
            close_count.fetch_add(1, Ordering::AcqRel);
            close_release.cancelled().await;
            match succeeds {
                true => Ok(()),
                false => Err(Error::NativeConnectionRetirementTimeout { timeout_ms: 1 }),
            }
        });
        // No task can poll the close primitive before a fenced command exists.
        assert_eq!(*status.borrow(), CloseState::Idle);
        assert_eq!(count.load(Ordering::Acquire), 0);
        assert!(mailbox.try_send(fence().commit()).is_ok());
        // All duplicates are generated through the real synchronous fence boundary.
        (0..8).for_each(|_| {
            let _duplicate = mailbox.try_send(fence().commit());
        });
        // The mailbox can retain at most one pending command even before actor scheduling.
        assert_eq!(mailbox.capacity(), 0);
        let expected_closing = close_step(CloseState::Idle, CloseEvent::Fenced).0;
        tokio::time::timeout(
            Duration::from_secs(1),
            status.wait_for(|s| *s == expected_closing),
        )
        .await
        .expect("actor scheduled")
        .expect("actor live");
        release.cancel();
        let outcome =
            tokio::time::timeout(Duration::from_secs(1), close_actor::outcome(status.clone()))
                .await
                .expect("actor terminates");
        let event = match succeeds {
            true => CloseEvent::CloseSucceeded,
            false => CloseEvent::CloseFailed,
        };
        assert_eq!(*status.borrow(), close_step(expected_closing, event).0);
        assert_eq!(Some(outcome), status.borrow().outcome());
        assert_eq!(count.load(Ordering::Acquire), 1);
    }
}

/// Sender disappearance is explicitly Unused and never polls the injected close future.
#[tokio::test]
async fn unused_actor_conforms_without_a_physical_close() {
    let polled = Arc::new(AtomicBool::new(false));
    let closing = Arc::clone(&polled);
    let (mailbox, status) =
        close_actor::spawn(&native_send_runtime().expect("runtime"), async move {
            closing.store(true, Ordering::Release);
            Ok(())
        });
    drop(mailbox);
    let outcome = tokio::time::timeout(Duration::from_secs(1), close_actor::outcome(status))
        .await
        .expect("actor terminates");
    assert_eq!(
        Some(outcome),
        close_step(CloseState::Idle, CloseEvent::ObserversGone)
            .0
            .outcome()
    );
    assert!(!polled.load(Ordering::Acquire));
}

/// Shutdown may interrupt idle or already-closing actors without publishing false success.
#[test]
fn executor_shutdown_conforms_before_and_after_close_start() {
    for begin in [false, true] {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let polled = Arc::new(AtomicBool::new(false));
        let closing = Arc::clone(&polled);
        let (mailbox, mut status) = close_actor::spawn(runtime.handle(), async move {
            closing.store(true, Ordering::Release);
            std::future::pending::<crate::error::Result<()>>().await
        });
        let state = match begin {
            true => {
                assert!(mailbox.try_send(fence().commit()).is_ok());
                runtime.block_on(async {
                    tokio::time::timeout(
                        Duration::from_secs(1),
                        status.wait_for(|s| *s == CloseState::Closing),
                    )
                    .await
                    .expect("scheduled")
                    .expect("live actor");
                });
                CloseState::Closing
            }
            false => CloseState::Idle,
        };
        drop(runtime);
        let outcome = futures::executor::block_on(close_actor::outcome(status));
        assert_eq!(
            Some(outcome),
            close_step(state, CloseEvent::RuntimeStopped).0.outcome()
        );
        assert_eq!(outcome, CloseOutcome::Interrupted);
        assert_eq!(polled.load(Ordering::Acquire), begin);
    }
}

/// The real lifecycle adapter reports close failure separately from task completion.
#[tokio::test]
async fn lifecycle_outcome_preserves_close_failure() {
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let _proof = permit.try_mark_irrevocable().expect("claim");
    let lifecycle = SendLifecycle::new(
        native_send_runtime().expect("runtime"),
        acceptance,
        fence(),
        async { Err(Error::NativeConnectionRetirementTimeout { timeout_ms: 1 }) },
    );
    lifecycle.fail();
    let outcome = tokio::time::timeout(Duration::from_secs(1), lifecycle.outcome())
        .await
        .expect("terminates");
    assert_eq!(outcome, CloseOutcome::Failed);
}

/// Unwinding in close IO destroys the reporter and cannot strand its observers.
#[tokio::test]
async fn physical_close_panic_publishes_interrupted() {
    let (mailbox, status) = close_actor::spawn(
        &native_send_runtime().expect("runtime"),
        std::future::poll_fn(|_| -> std::task::Poll<crate::error::Result<()>> {
            panic!("injected physical-close panic")
        }),
    );
    assert!(mailbox.try_send(fence().commit()).is_ok());
    let outcome = tokio::time::timeout(Duration::from_secs(1), close_actor::outcome(status))
        .await
        .expect("panic cannot strand a waiter");
    assert_eq!(outcome, CloseOutcome::Interrupted);
}
