use std::sync::atomic::AtomicBool;

use super::send_runtime::RetirementFenceGuard;
use super::*;

async fn wait_for_flag(flag: &AtomicBool, label: &str) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !flag.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
}

fn test_retirement_fence() -> NativeRetirementFence {
    NativeRetirementFence::new(ConnectionStateCell::new(), CancellationToken::new())
}

#[test]
fn connection_wide_fence_serializes_cross_channel_admission_with_retirement() {
    let connection_state = ConnectionStateCell::new();
    let cancellation = CancellationToken::new();
    let fence = NativeRetirementFence::new(connection_state.clone(), cancellation.clone());
    let first_permit = SendPermit::always();
    let first_acceptance = first_permit.acceptance();
    let (admitted_sender, admitted_receiver) = std::sync::mpsc::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let (first_done_sender, first_done_receiver) = std::sync::mpsc::channel();
    let first_channel = {
        let fence = fence.clone();
        std::thread::spawn(move || {
            let _admission = fence
                .try_send_admission()
                .expect("the first channel must linearize before retirement");
            let proof = first_permit
                .try_mark_irrevocable()
                .expect("the first channel permit must remain live");
            admitted_sender
                .send(())
                .expect("test admission observer must remain open");
            release_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("test admission release must arrive");
            proof.mark_accepted();
            first_done_sender
                .send(())
                .expect("first-channel completion observer must remain open");
        })
    };
    admitted_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("the first channel must hold the connection-wide gate");

    let (retirement_waiting_sender, retirement_waiting_receiver) = std::sync::mpsc::channel();
    let (retirement_done_sender, retirement_done_receiver) = std::sync::mpsc::channel();
    let retirement = {
        let fence = fence.clone();
        std::thread::spawn(move || {
            fence.request_with_observer_for_test(|| {
                retirement_waiting_sender
                    .send(())
                    .expect("retirement boundary observer must remain open");
            });
            retirement_done_sender
                .send(())
                .expect("retirement completion observer must remain open");
        })
    };
    retirement_waiting_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("retirement must reach the actual fence boundary");
    assert_eq!(fence.waiting_retirements_for_test(), 1);

    release_sender
        .send(())
        .expect("the first channel must still be waiting");
    first_done_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("the first channel must finish after release");
    retirement_done_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("retirement must finish after admission releases the gate");
    first_channel
        .join()
        .expect("the first channel must not panic");
    retirement.join().expect("retirement must not panic");

    let second_permit = SendPermit::always();
    let second_acceptance = second_permit.acceptance();
    assert!(fence.try_send_admission().is_none());
    assert!(second_acceptance.try_cancel());
    assert!(first_acceptance.is_accepted());
    assert_eq!(
        connection_state.snapshot().webrtc(),
        WebrtcConnectionState::Closed
    );
    assert!(cancellation.is_cancelled());
}

#[test]
fn cancellation_at_native_final_gate_does_not_retire_the_connection() {
    let connection_state = ConnectionStateCell::new();
    let cancellation = CancellationToken::new();
    let fence = NativeRetirementFence::new(connection_state.clone(), cancellation.clone());
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    assert!(acceptance.try_cancel());
    let mut retirement = RetirementFenceGuard::new(fence.clone());
    let admission = fence
        .try_send_admission()
        .expect("an open connection must expose its final admission gate");

    assert!(permit.try_mark_irrevocable().is_none());
    drop(admission);
    retirement.disarm();
    drop(retirement);

    assert_ne!(
        connection_state.snapshot().webrtc(),
        WebrtcConnectionState::Closed
    );
    assert!(!cancellation.is_cancelled());
}

#[test]
fn panic_before_native_permit_claim_does_not_retire_the_connection() {
    let connection_state = ConnectionStateCell::new();
    let cancellation = CancellationToken::new();
    let fence = NativeRetirementFence::new(connection_state.clone(), cancellation.clone());
    let permit = SendPermit::always().with_irrevocable_guard(|_| {
        panic!("injected pre-claim panic");
    });
    let acceptance = permit.acceptance();
    let retirement = RetirementFenceGuard::once_irrevocable(fence.clone(), acceptance.clone());
    let admission = fence
        .try_send_admission()
        .expect("an open connection must expose its final admission gate");

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _proof = permit.try_mark_irrevocable();
    }));
    drop(admission);
    drop(retirement);

    assert!(outcome.is_err());
    assert!(acceptance.try_cancel());
    assert_ne!(
        connection_state.snapshot().webrtc(),
        WebrtcConnectionState::Closed
    );
    assert!(!cancellation.is_cancelled());
}

struct PanicOnFirstPoll;

impl std::future::Future for PanicOnFirstPoll {
    type Output = Result<()>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        panic!("injected first-poll panic");
    }
}

#[test]
fn first_poll_panic_releases_admission_before_retirement() {
    let connection_state = ConnectionStateCell::new();
    let cancellation = CancellationToken::new();
    let fence = NativeRetirementFence::new(connection_state.clone(), cancellation.clone());
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let failure_fence = fence.clone();
        let mut retirement = IrrevocableSendGuard::new(acceptance.clone(), move || {
            failure_fence.request();
        });
        let admission = fence
            .try_send_admission()
            .expect("an open connection must expose its final admission gate");
        retirement.bind(
            permit
                .try_mark_irrevocable()
                .expect("live permit must become irrevocable"),
        );
        let mut send = Box::pin(PanicOnFirstPoll);
        let _result = poll_once_while_guarded(send.as_mut(), admission);
    }));

    assert!(outcome.is_err());
    assert!(acceptance.is_irrevocable());
    assert!(!acceptance.is_accepted());
    assert_eq!(
        connection_state.snapshot().webrtc(),
        WebrtcConnectionState::Closed
    );
    assert!(cancellation.is_cancelled());
}

struct FenceAwarePending {
    cancellation: CancellationToken,
    fenced_before_drop: Arc<AtomicBool>,
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[tokio::test]
async fn cancelled_revocable_native_send_releases_task_owned_state() {
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let retired = Arc::new(AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let send = {
        let started = Arc::clone(&started);
        let dropped = Arc::clone(&dropped);
        let release = Arc::clone(&release);
        async move {
            let _owned_state = DropFlag(dropped);
            started.store(true, Ordering::Release);
            release.notified().await;
            let _proof = permit
                .try_mark_irrevocable()
                .ok_or(Error::SendPermitRevoked)?;
            Ok(())
        }
    };
    let retirement = {
        let retired = Arc::clone(&retired);
        async move {
            retired.store(true, Ordering::Release);
            Ok(())
        }
    };
    let runtime = native_send_runtime().expect("Tokio test runtime must be available");
    let caller = tokio::spawn({
        let acceptance = acceptance.clone();
        async move {
            run_send_with_retirement(
                &runtime,
                acceptance,
                test_retirement_fence(),
                send,
                retirement,
            )
            .await
        }
    });
    wait_for_flag(&started, "revocable native send start").await;

    caller.abort();
    let _cancelled = caller.await;
    wait_for_flag(&dropped, "revocable native task state release").await;

    assert!(!acceptance.is_irrevocable());
    assert!(!retired.load(Ordering::Acquire));
}

impl std::future::Future for FenceAwarePending {
    type Output = Result<()>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::task::Poll::Pending
    }
}

impl Drop for FenceAwarePending {
    fn drop(&mut self) {
        self.fenced_before_drop
            .store(self.cancellation.is_cancelled(), Ordering::Release);
    }
}

#[tokio::test]
async fn started_native_send_outlives_cancelled_caller() {
    let started = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    let send = {
        let started = Arc::clone(&started);
        let completed = Arc::clone(&completed);
        let release = Arc::clone(&release);
        async move {
            started.store(true, Ordering::Release);
            release.notified().await;
            completed.store(true, Ordering::Release);
            Ok(())
        }
    };
    let runtime = native_send_runtime().expect("Tokio test runtime must be available");
    let caller =
        tokio::spawn(
            async move { run_irrevocable_send(&runtime, test_retirement_fence(), send).await },
        );
    wait_for_flag(&started, "native send start").await;

    caller.abort();
    release.notify_one();
    wait_for_flag(&completed, "native send completion").await;

    assert!(completed.load(Ordering::Acquire));
}

#[tokio::test]
async fn native_close_witness_outlives_cancelled_waiter() {
    let started = Arc::new(AtomicBool::new(false));
    let close_completed = Arc::new(AtomicBool::new(false));
    let physical_close_completed = Arc::new(AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    let close = {
        let started = Arc::clone(&started);
        let close_completed = Arc::clone(&close_completed);
        let release = Arc::clone(&release);
        async move {
            started.store(true, Ordering::Release);
            release.notified().await;
            close_completed.store(true, Ordering::Release);
            Ok(())
        }
    };
    let runtime = native_send_runtime().expect("Tokio test runtime must be available");
    let witness = Arc::clone(&physical_close_completed);
    let waiter =
        tokio::spawn(async move { run_native_close_with_witness(&runtime, close, witness).await });
    wait_for_flag(&started, "native close start").await;

    waiter.abort();
    release.notify_one();
    wait_for_flag(&close_completed, "native physical close completion").await;
    wait_for_flag(&physical_close_completed, "native physical close witness").await;
}

#[tokio::test]
async fn failed_irrevocable_send_retires_after_caller_is_cancelled() {
    let started = Arc::new(AtomicBool::new(false));
    let retired = Arc::new(AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let send = {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        async move {
            let _permit = permit
                .try_mark_irrevocable()
                .expect("live permit must become irrevocable");
            started.store(true, Ordering::Release);
            release.notified().await;
            Err::<(), _>(Error::MessageNotDelivered(
                "injected post-cancellation failure".to_string(),
            ))
        }
    };
    let retirement = {
        let retired = Arc::clone(&retired);
        async move {
            retired.store(true, Ordering::Release);
            Ok(())
        }
    };
    let runtime = native_send_runtime().expect("Tokio test runtime must be available");
    let caller = tokio::spawn(async move {
        run_send_with_retirement(
            &runtime,
            acceptance,
            test_retirement_fence(),
            send,
            retirement,
        )
        .await
    });
    wait_for_flag(&started, "irrevocable native send start").await;

    caller.abort();
    release.notify_one();
    wait_for_flag(&retired, "native retirement").await;

    assert!(retired.load(Ordering::Acquire));
}

#[tokio::test]
async fn accepted_send_is_not_retired_when_its_waiter_is_cancelled() {
    let accepted = Arc::new(AtomicBool::new(false));
    let retired = Arc::new(AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let send = {
        let accepted = Arc::clone(&accepted);
        let release = Arc::clone(&release);
        async move {
            permit
                .try_mark_irrevocable()
                .expect("live permit must become irrevocable")
                .mark_accepted();
            accepted.store(true, Ordering::Release);
            release.notified().await;
            Ok(())
        }
    };
    let retirement = {
        let retired = Arc::clone(&retired);
        async move {
            retired.store(true, Ordering::Release);
            Ok(())
        }
    };
    let runtime = native_send_runtime().expect("Tokio test runtime must be available");
    let caller = tokio::spawn(async move {
        run_send_with_retirement(
            &runtime,
            acceptance,
            test_retirement_fence(),
            send,
            retirement,
        )
        .await
    });
    wait_for_flag(&accepted, "native send acceptance").await;

    caller.abort();
    let _cancelled = caller.await;
    tokio::task::yield_now().await;

    assert!(!retired.load(Ordering::Acquire));
    release.notify_one();
}

#[tokio::test]
async fn cancellation_wins_before_native_background_send_becomes_irrevocable() {
    let attempted_write = Arc::new(AtomicBool::new(false));
    let retired = Arc::new(AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let send = {
        let attempted_write = Arc::clone(&attempted_write);
        let release = Arc::clone(&release);
        async move {
            release.notified().await;
            let Some(_permit) = permit.try_mark_irrevocable() else {
                return Err(Error::SendPermitRevoked);
            };
            attempted_write.store(true, Ordering::Release);
            Ok(())
        }
    };
    let retirement = {
        let retired = Arc::clone(&retired);
        async move {
            retired.store(true, Ordering::Release);
            Ok(())
        }
    };
    let runtime = native_send_runtime().expect("Tokio test runtime must be available");
    let run = run_send_with_retirement(
        &runtime,
        acceptance.clone(),
        test_retirement_fence(),
        send,
        retirement,
    );
    assert!(acceptance.try_cancel());
    release.notify_one();

    assert!(matches!(run.await, Err(Error::SendPermitRevoked)));
    assert!(!attempted_write.load(Ordering::Acquire));
    assert!(!retired.load(Ordering::Acquire));
}

#[test]
fn missing_runtime_returns_typed_error() {
    assert!(matches!(
        native_send_runtime(),
        Err(Error::NativeSendRuntimeUnavailable)
    ));
}

#[tokio::test]
async fn native_send_task_preserves_join_error_source() {
    let runtime = native_send_runtime().expect("Tokio test runtime must be available");
    let error = run_irrevocable_send::<()>(&runtime, test_retirement_fence(), async {
        panic!("native send task panic witness");
    })
    .await
    .expect_err("panicking send task must fail");

    assert!(matches!(&error, Error::NativeSendTask(source) if source.is_panic()));
    assert!(std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<tokio::task::JoinError>())
        .is_some_and(tokio::task::JoinError::is_panic));
}

#[tokio::test]
async fn panicking_irrevocable_native_send_retires_connection() {
    let retired = Arc::new(AtomicBool::new(false));
    let connection_state = ConnectionStateCell::new();
    let cancellation = CancellationToken::new();
    let retirement_fence =
        NativeRetirementFence::new(connection_state.clone(), cancellation.clone());
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let send = async move {
        let _permit = permit
            .try_mark_irrevocable()
            .expect("live permit must become irrevocable");
        panic!("irrevocable native send panic witness");
    };
    let retirement = {
        let retired = Arc::clone(&retired);
        async move {
            retired.store(true, Ordering::Release);
            Ok(())
        }
    };
    let runtime = native_send_runtime().expect("Tokio test runtime must be available");

    let error =
        run_send_with_retirement::<()>(&runtime, acceptance, retirement_fence, send, retirement)
            .await
            .expect_err("panicking send task must fail");

    assert!(retired.load(Ordering::Acquire));
    assert_eq!(
        connection_state.snapshot().webrtc(),
        WebrtcConnectionState::Closed
    );
    assert!(cancellation.is_cancelled());
    assert!(matches!(
        &error,
        Error::NativeSendPanic(message) if message == "irrevocable native send panic witness"
    ));
}

#[tokio::test]
async fn panicking_revocable_native_send_does_not_retire_connection() {
    let retired = Arc::new(AtomicBool::new(false));
    let connection_state = ConnectionStateCell::new();
    let cancellation = CancellationToken::new();
    let retirement_fence =
        NativeRetirementFence::new(connection_state.clone(), cancellation.clone());
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let send = async move {
        drop(permit);
        panic!("revocable native send panic witness");
    };
    let retirement = {
        let retired = Arc::clone(&retired);
        async move {
            retired.store(true, Ordering::Release);
            Ok(())
        }
    };
    let runtime = native_send_runtime().expect("Tokio test runtime must be available");

    let error =
        run_send_with_retirement::<()>(&runtime, acceptance, retirement_fence, send, retirement)
            .await
            .expect_err("panicking send task must fail");

    assert!(!retired.load(Ordering::Acquire));
    assert_eq!(
        connection_state.snapshot().webrtc(),
        WebrtcConnectionState::New
    );
    assert!(!cancellation.is_cancelled());
    assert!(matches!(
        &error,
        Error::NativeSendPanic(message) if message == "revocable native send panic witness"
    ));
}

#[tokio::test]
async fn irrevocable_native_send_has_a_completion_bound() {
    let runtime = native_send_runtime().expect("Tokio test runtime must be available");
    let cancellation = CancellationToken::new();
    let fenced_before_drop = Arc::new(AtomicBool::new(false));
    let send = FenceAwarePending {
        cancellation: cancellation.clone(),
        fenced_before_drop: Arc::clone(&fenced_before_drop),
    };
    let error = run_irrevocable_send_with_timeout(
        &runtime,
        NATIVE_SEND_TEST_COMPLETION_TIMEOUT,
        NativeRetirementFence::new(ConnectionStateCell::new(), cancellation),
        send,
    )
    .await
    .expect_err("an irrevocable send must not remain pending forever");

    assert!(matches!(
        error,
        Error::NativeSendCompletionTimeout { timeout_ms }
            if timeout_ms == NATIVE_SEND_TEST_COMPLETION_TIMEOUT.as_millis()
    ));
    assert!(fenced_before_drop.load(Ordering::Acquire));
}

#[tokio::test]
async fn failed_irrevocable_send_retires_before_preserving_its_error() {
    let retired = Arc::new(AtomicBool::new(false));
    let connection_state = ConnectionStateCell::new();
    let cancellation = CancellationToken::new();
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();
    let send = async move {
        let _proof = permit
            .try_mark_irrevocable()
            .expect("live permit must become irrevocable");
        Err::<(), _>(Error::NativeSendCompletionTimeout { timeout_ms: 1 })
    };
    let retirement = {
        let retired = Arc::clone(&retired);
        async move {
            retired.store(true, Ordering::Release);
            Ok(())
        }
    };
    let runtime = native_send_runtime().expect("Tokio test runtime must be available");
    let retirement_fence =
        NativeRetirementFence::new(connection_state.clone(), cancellation.clone());

    let result =
        run_send_with_retirement(&runtime, acceptance, retirement_fence, send, retirement).await;

    assert!(retired.load(Ordering::Acquire));
    assert_eq!(
        connection_state.snapshot().webrtc(),
        WebrtcConnectionState::Closed
    );
    assert!(cancellation.is_cancelled());
    assert!(matches!(
        result,
        Err(Error::NativeSendCompletionTimeout { timeout_ms: 1 })
    ));
}
