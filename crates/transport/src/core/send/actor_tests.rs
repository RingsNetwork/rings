//! Observable traces of the real interpreter, independent of the reducer's expected output.

use std::cell::Cell;
use std::cell::RefCell;
use std::future::ready;
use std::future::Future;
use std::rc::Rc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use super::actor;
use super::model::CloseEvent;
use super::model::CloseOutcome;
use super::model::CloseState;
use crate::error::Error;

/// Successful and failed actors publish a terminal snapshot once, including destruction.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn terminal_publications_match_effects_without_drop_duplicates() {
    for succeeds in [false, true] {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&trace);
        let close = ready(if succeeds {
            Ok(())
        } else {
            Err(Error::SendPermitRevoked)
        });
        let mut actor = Box::pin(actor::run(ready(CloseEvent::Fenced), close, move |state| {
            observed.borrow_mut().push(state)
        }));
        let mut context = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut context).is_ready());
        drop(actor);
        assert_eq!(*trace.borrow(), vec![
            CloseState::Closing,
            CloseState::Finished(if succeeds {
                CloseOutcome::Succeeded
            } else {
                CloseOutcome::Failed
            })
        ]);
    }
    let trace = Rc::new(RefCell::new(Vec::new()));
    let observed = Rc::clone(&trace);
    let mut actor = Box::pin(actor::run(
        ready(CloseEvent::ObserversGone),
        ready(Ok(())),
        move |state| observed.borrow_mut().push(state),
    ));
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    drop(actor);
    assert_eq!(*trace.borrow(), vec![CloseState::Finished(
        CloseOutcome::Unused
    )]);
}

/// One close initiation permits repeated Future polls but never duplicate Closing publication.
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), test)]
fn repeated_close_polls_and_interruption_have_exact_publication_traces() {
    for complete in [false, true] {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&trace);
        let polls = Rc::new(Cell::new(0));
        let polled = Rc::clone(&polls);
        let close = std::future::poll_fn(move |_| {
            polled.set(polled.get() + 1);
            if polled.get() == 1 {
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        });
        let mut actor = Box::pin(actor::run(ready(CloseEvent::Fenced), close, move |state| {
            observed.borrow_mut().push(state)
        }));
        let mut context = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        if complete {
            assert!(actor.as_mut().poll(&mut context).is_ready());
        }
        drop(actor);
        assert_eq!(polls.get(), if complete { 2 } else { 1 });
        assert_eq!(*trace.borrow(), vec![
            CloseState::Closing,
            CloseState::Finished(if complete {
                CloseOutcome::Succeeded
            } else {
                CloseOutcome::Interrupted
            })
        ]);
    }
    let trace = Rc::new(RefCell::new(Vec::new()));
    let observed = Rc::clone(&trace);
    let actor = actor::run(ready(CloseEvent::Fenced), ready(Ok(())), move |state| {
        observed.borrow_mut().push(state)
    });
    drop(actor);
    assert_eq!(*trace.borrow(), vec![CloseState::Finished(
        CloseOutcome::Interrupted
    )]);
}
