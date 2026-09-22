//! Browser mailbox and local-executor effects for the shared send and close protocol.

use std::cell::Cell;
use std::future::Future;
use std::rc::Rc;

use futures_channel::oneshot;
use futures_util::future::LocalBoxFuture;
use futures_util::future::Shared;
use futures_util::FutureExt;
use wasm_bindgen_futures::spawn_local;
use web_sys::RtcPeerConnection;

use crate::core::send::actor;
use crate::core::send::lifecycle::Retirement;
use crate::core::send::lifecycle::SendLifecycle;
use crate::core::send::model::CloseEvent;
use crate::core::send::model::CloseOutcome;
use crate::core::send::model::CloseState;
use crate::core::transport::ConnectionStateCell;
use crate::core::transport::SendAcceptance;
use crate::error::Result;

/// Evidence that the browser generation was logically fenced before command delivery.
pub(super) struct FencedCommand {
    /// Only this adapter can construct evidence of browser logical closure.
    _sealed: (),
}

/// Linear mailbox address: the only command consumes its sender, coalescing duplicates.
enum Mailbox {
    /// Exactly one retirement command may be sent.
    Open(oneshot::Sender<FencedCommand>),
    /// The retirement command was already submitted; duplicates stutter.
    Submitted,
}

/// Browser-specific capabilities; no physical-close future or actor state lives here.
pub(super) struct BrowserRetirement {
    /// Logical fence rejects later sends synchronously, before any resource release.
    connection_state: ConnectionStateCell,
    /// Cell moves the unique sender without borrowing across a callback or await.
    mailbox: Cell<Mailbox>,
    /// Repeatable completion observer; cloning it never owns or cancels close IO.
    completion: Shared<LocalBoxFuture<'static, CloseOutcome>>,
    /// Monotone evidence that this send requested cleanup.
    requested: Cell<bool>,
}

/// Browser specialization of the same policy used by native SendLifecycle.
pub(super) type BrowserLifecycle = SendLifecycle<BrowserRetirement>;

impl Retirement for BrowserRetirement {
    type Command = FencedCommand;
    type Completion = Shared<LocalBoxFuture<'static, CloseOutcome>>;
    fn requested(&self) -> bool {
        self.requested.get()
    }
    fn completion(&self) -> Self::Completion {
        self.completion.clone()
    }
    fn fence(&self) -> Self::Command {
        self.connection_state.close();
        FencedCommand { _sealed: () }
    }
    fn notify(&self, command: Self::Command) {
        self.requested.set(true);
        match self.mailbox.replace(Mailbox::Submitted) {
            Mailbox::Open(sender) => {
                let _delivery = sender.send(command);
            }
            Mailbox::Submitted => (),
        }
    }
}

impl BrowserLifecycle {
    /// Create the same close actor using a local mailbox and spawn_local executor.
    pub(super) fn new(
        acceptance: SendAcceptance,
        connection_state: ConnectionStateCell,
        connection: RtcPeerConnection,
    ) -> Rc<Self> {
        // Browser close is synchronous. Success means the local API returned,
        // not that a remote endpoint acknowledged closure or drained queued data.
        let close = async move {
            connection.close();
            Ok(())
        };
        Self::spawn(acceptance, connection_state, close).0
    }

    /// Wire browser effects to the common actor; snapshots are read-only values for observers.
    fn spawn(
        acceptance: SendAcceptance,
        connection_state: ConnectionStateCell,
        close: impl Future<Output = Result<()>> + 'static,
    ) -> (Rc<Self>, Rc<Cell<CloseState>>) {
        let (sender, receiver) = oneshot::channel();
        // Completion is separate from the command channel and supports multiple waiters.
        let (completed, completion) = oneshot::channel();
        let completion = completion
            .map(|result| result.unwrap_or(CloseOutcome::Interrupted))
            .boxed_local()
            .shared();
        // A terminal reducer effect consumes this publication capability exactly once.
        let completed = Cell::new(Some(completed));
        let status = Rc::new(Cell::new(CloseState::Idle));
        let published = Rc::clone(&status);
        let inbox = async move {
            receiver
                .await
                .map_or(CloseEvent::ObserversGone, |_| CloseEvent::Fenced)
        };
        spawn_local(actor::run(inbox, close, move |state| {
            published.set(state);
            if let Some(outcome) = state.outcome() {
                if let Some(sender) = completed.take() {
                    let _published = sender.send(outcome);
                }
                tracing::debug!(?outcome, "browser close actor finished");
            }
        }));
        let lifecycle = Rc::new(Self::with_adapter(acceptance, BrowserRetirement {
            connection_state,
            mailbox: Cell::new(Mailbox::Open(sender)),
            completion,
            requested: Cell::new(false),
        }));
        (lifecycle, status)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use wasm_bindgen_test::wasm_bindgen_test;

    use super::*;
    use crate::core::send::model::CloseOutcome;
    use crate::core::send::operation::test_queue;
    use crate::core::send::owner::OwnedSend;
    use crate::core::transport::SendPermit;
    use crate::core::transport::WebrtcConnectionState;
    use crate::error::Error;

    /// Yield one microtask turn to the real browser-local actor scheduler.
    async fn actor_turn() {
        wasm_bindgen_futures::JsFuture::from(js_sys::Promise::resolve(
            &wasm_bindgen::JsValue::UNDEFINED,
        ))
        .await
        .expect("microtask resolves");
    }

    /// Browser delivery coalesces duplicates and fences before the common actor performs IO.
    #[wasm_bindgen_test]
    async fn browser_actor_fences_then_closes_once_for_duplicate_observers() {
        let permit = SendPermit::always();
        let acceptance = permit.acceptance();
        let _proof = permit.try_mark_irrevocable().expect("claim");
        let state = ConnectionStateCell::new();
        let calls = Rc::new(Cell::new(0));
        let observed = Rc::clone(&calls);
        let (release, wait) = oneshot::channel::<()>();
        let (lifecycle, status) = BrowserLifecycle::spawn(acceptance, state.clone(), async move {
            observed.set(observed.get() + 1);
            wait.await.expect("close released");
            Ok(())
        });
        (0..8).for_each(|_| lifecycle.fail());
        assert_eq!(state.snapshot().webrtc(), WebrtcConnectionState::Closed);
        assert_eq!(calls.get(), 0, "fencing precedes scheduled physical IO");
        actor_turn().await;
        assert_eq!(status.get(), CloseState::Closing);
        assert_eq!(calls.get(), 1);
        drop(lifecycle); // Caller disappearance cannot cancel the actor's close.
        release.send(()).expect("actor still owns close");
        actor_turn().await;
        assert_eq!(status.get().outcome(), Some(CloseOutcome::Succeeded));
        assert_eq!(calls.get(), 1);
    }

    /// Unused and failed close remain distinct under the browser scheduler too.
    #[wasm_bindgen_test]
    async fn browser_actor_preserves_unused_and_failed_outcomes() {
        for requested in [false, true] {
            let permit = SendPermit::always();
            let acceptance = permit.acceptance();
            let _proof = permit.try_mark_irrevocable().expect("claim");
            let calls = Rc::new(Cell::new(0));
            let observed = Rc::clone(&calls);
            let (lifecycle, status) =
                BrowserLifecycle::spawn(acceptance, ConnectionStateCell::new(), async move {
                    observed.set(observed.get() + 1);
                    Err(Error::SendPermitRevoked)
                });
            if requested {
                lifecycle.fail();
            }
            drop(lifecycle);
            actor_turn().await;
            assert_eq!(calls.get(), usize::from(requested));
            assert_eq!(
                status.get().outcome(),
                Some(if requested {
                    CloseOutcome::Failed
                } else {
                    CloseOutcome::Unused
                })
            );
        }
    }

    /// The browser uses the real common owner for acceptance and checked byte accounting.
    #[wasm_bindgen_test]
    async fn browser_queue_owner_preserves_success_failure_and_overflow() {
        for succeeds in [false, true] {
            let permit = SendPermit::always();
            let acceptance = permit.acceptance();
            let state = ConnectionStateCell::new();
            let (lifecycle, status) =
                BrowserLifecycle::spawn(acceptance.clone(), state.clone(), async { Ok(()) });
            let enqueued = Arc::new(AtomicU64::new(7));
            let primitive = async move {
                if succeeds {
                    Ok(())
                } else {
                    Err(Error::SendPermitRevoked)
                }
            };
            let queue = test_queue(primitive, permit, (), Arc::clone(&enqueued), 3)
                .expect("checked offset");
            let result = OwnedSend::new(queue, lifecycle).await;
            assert_eq!(result.is_ok(), succeeds);
            assert_eq!(acceptance.is_accepted(), succeeds);
            assert_eq!(
                enqueued.load(Ordering::SeqCst),
                if succeeds { 10 } else { 7 }
            );
            assert_eq!(
                state.snapshot().webrtc() == WebrtcConnectionState::Closed,
                !succeeds
            );
            actor_turn().await;
            assert_eq!(
                status.get().outcome(),
                Some(if succeeds {
                    CloseOutcome::Unused
                } else {
                    CloseOutcome::Succeeded
                })
            );
        }
        let permit = SendPermit::always();
        let acceptance = permit.acceptance();
        let queue = test_queue(
            async { Ok(()) },
            permit,
            (),
            Arc::new(AtomicU64::new(u64::MAX)),
            1,
        );
        assert!(matches!(queue, Err(Error::SendByteCountOverflow)));
        assert!(!acceptance.is_irrevocable());
    }
    /// Error return waits for actor completion; cancelled and repeated waiters share its result.
    #[wasm_bindgen_test]
    async fn browser_error_waits_for_cleanup_without_owning_it() {
        use std::task::Context;
        use std::task::Waker;
        let permit = SendPermit::always();
        let acceptance = permit.acceptance();
        let _proof = permit.try_mark_irrevocable().expect("claim");
        let (release, wait) = oneshot::channel::<()>();
        let (lifecycle, status) =
            BrowserLifecycle::spawn(acceptance, ConnectionStateCell::new(), async move {
                wait.await.expect("close release");
                Ok(())
            });
        lifecycle.fail();
        let mut first = Box::pin(lifecycle.finish::<()>(Err(Error::SendPermitRevoked)));
        let mut context = Context::from_waker(Waker::noop());
        assert!(first.as_mut().poll(&mut context).is_pending());
        actor_turn().await;
        assert_eq!(status.get(), CloseState::Closing);
        assert!(first.as_mut().poll(&mut context).is_pending());
        drop(first);
        let mut second = Box::pin(lifecycle.finish::<()>(Err(Error::SendPermitRevoked)));
        assert!(second.as_mut().poll(&mut context).is_pending());
        release
            .send(())
            .expect("actor survived waiter cancellation");
        assert!(matches!(second.await, Err(Error::SendPermitRevoked)));
        assert_eq!(lifecycle.outcome().await, CloseOutcome::Succeeded);
        assert_eq!(status.get(), CloseState::Finished(CloseOutcome::Succeeded));
        assert!(matches!(
            lifecycle.finish::<()>(Err(Error::SendPermitRevoked)).await,
            Err(Error::SendPermitRevoked)
        ));
    }

    /// Constructing two synchronous send futures does not reserve the same byte snapshot twice.
    #[wasm_bindgen_test]
    async fn browser_sync_entry_snapshots_offsets_only_when_executing() {
        use crate::core::send::operation::send_sync;
        let counter = Arc::new(AtomicU64::new(0));
        let first_permit = SendPermit::always();
        let second_permit = SendPermit::always();
        let (first_owner, _) = BrowserLifecycle::spawn(
            first_permit.acceptance(),
            ConnectionStateCell::new(),
            async { Ok(()) },
        );
        let (second_owner, _) = BrowserLifecycle::spawn(
            second_permit.acceptance(),
            ConnectionStateCell::new(),
            async { Ok(()) },
        );
        let first = send_sync(
            || Ok(()),
            first_permit,
            Arc::clone(&counter),
            10,
            first_owner,
        );
        let second = send_sync(
            || Ok(()),
            second_permit,
            Arc::clone(&counter),
            20,
            second_owner,
        );
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert_eq!(first.await.expect("first accepted"), 10);
        assert_eq!(second.await.expect("second accepted"), 30);
        assert_eq!(counter.load(Ordering::SeqCst), 30);
    }
}
