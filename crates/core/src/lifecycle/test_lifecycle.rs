use super::*;

#[test]
fn test_stop_source_propagates_stop_to_existing_and_cloned_tokens() {
    let source = StopSource::new();
    let first = source.token();
    let second = first.clone();

    assert!(!first.should_stop());
    assert!(!second.should_stop());
    assert!(!source.is_stop_requested());

    source.request_stop();

    assert!(first.should_stop());
    assert!(second.should_stop());
    assert!(source.is_stop_requested());
}

#[test]
fn test_cloned_stop_source_controls_the_same_lifecycle_scope() {
    let source = StopSource::new();
    let cloned_source = source.clone();
    let token = source.token();

    cloned_source.request_stop();

    assert!(token.should_stop());
    assert!(source.is_stop_requested());
}

#[test]
fn test_never_token_is_independent_from_other_sources() {
    let source = StopSource::new();
    let token = StopToken::never();

    source.request_stop();

    assert!(!token.should_stop());
}

#[cfg(all(not(target_family = "wasm"), not(miri)))]
#[tokio::test]
async fn test_stopped_wakes_existing_and_cloned_waiters() {
    let source = StopSource::new();
    let first = source.token();
    let second = first.clone();

    let waiters = tokio::spawn(async move {
        tokio::join!(first.stopped(), second.stopped());
    });
    tokio::task::yield_now().await;
    source.request_stop();

    tokio::time::timeout(std::time::Duration::from_secs(1), waiters)
        .await
        .expect("stop waiters wake before deadline")
        .expect("stop waiter task completes");
}

#[test]
fn test_stopped_wakes_registered_existing_and_cloned_futures() {
    use std::future::Future;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Wake;
    use std::task::Waker;

    struct CountingWaker(AtomicUsize);
    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let source = StopSource::new();
    let first = source.token();
    let second = first.clone();
    let mut first_stopped = Box::pin(first.stopped());
    let mut second_stopped = Box::pin(second.stopped());

    let wake_count = Arc::new(CountingWaker(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&wake_count));
    let mut cx = Context::from_waker(&waker);

    assert_eq!(first_stopped.as_mut().poll(&mut cx), Poll::Pending);
    assert_eq!(second_stopped.as_mut().poll(&mut cx), Poll::Pending);
    assert_eq!(wake_count.0.load(Ordering::SeqCst), 0);

    source.request_stop();
    assert!(wake_count.0.load(Ordering::SeqCst) >= 2);
    assert_eq!(first_stopped.as_mut().poll(&mut cx), Poll::Ready(()));
    assert_eq!(second_stopped.as_mut().poll(&mut cx), Poll::Ready(()));
}

#[test]
fn test_stopped_observes_a_request_before_registration() {
    let source = StopSource::new();
    let token = source.token();
    source.request_stop();

    futures::executor::block_on(token.stopped());
}
