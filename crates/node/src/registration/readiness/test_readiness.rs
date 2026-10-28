//! The laws of registration readiness (#866): a task is ready once a wait's receipts all
//! resolve stored, never otherwise; a newer wait ends an older one; `all` resolves once every
//! task is ready, at once for none. Every wait is woken by its receipts, never by time.

use std::sync::Arc;

use futures::channel::oneshot;
use futures::FutureExt;
use rings_core::swarm::storage_api::StoreUnconfirmed;

use super::RegistrationReadiness;

/// A receipt the test resolves through the returned sender.
fn receipt() -> (
    oneshot::Sender<Result<(), StoreUnconfirmed>>,
    impl std::future::Future<Output = Result<(), StoreUnconfirmed>> + 'static,
) {
    let (resolve, resolved) = oneshot::channel();
    (
        resolve,
        resolved.map(|outcome| outcome.unwrap_or(Err(StoreUnconfirmed::Abandoned))),
    )
}

/// A node with no registration is ready at once.
#[test]
fn test_no_registration_is_ready_at_once() {
    let readiness = Arc::new(RegistrationReadiness::new(0));
    assert!(readiness.all().now_or_never().is_some());
}

/// A task is ready once every receipt of its wait resolved stored, and `all` once every task.
#[tokio::test]
async fn test_a_task_is_ready_once_every_receipt_is_stored() {
    let readiness = Arc::new(RegistrationReadiness::new(2));
    let mut all = Box::pin(readiness.all());
    let (first, first_receipt) = receipt();
    let (second, second_receipt) = receipt();
    let wait = readiness.wait(0, vec![first_receipt, second_receipt]);
    let mut wait = Box::pin(wait);

    first.send(Ok(())).unwrap();
    assert!(
        (&mut wait).now_or_never().is_none(),
        "one receipt is unresolved"
    );
    assert!(!readiness.is_ready(0));
    second.send(Ok(())).unwrap();
    wait.await;
    assert!(readiness.is_ready(0));
    assert!((&mut all).now_or_never().is_none(), "task 1 is not ready");

    readiness.wait(1, Vec::<std::future::Ready<_>>::new()).await;
    assert!(readiness.is_ready(1));
    all.await;
}

/// A receipt that resolves otherwise than stored ends the wait without making the task ready.
#[tokio::test]
async fn test_an_unconfirmed_receipt_leaves_the_task_unready() {
    let readiness = Arc::new(RegistrationReadiness::new(1));
    let (refused, refused_receipt) = receipt();
    let wait = readiness.wait(0, vec![refused_receipt]);
    refused.send(Err(StoreUnconfirmed::Untracked)).unwrap();
    wait.await;
    assert!(!readiness.is_ready(0));
}

/// The one-wait law: a newer wait of a task ends its older one, which sets nothing even if its
/// receipts resolve stored after.
#[tokio::test]
async fn test_a_newer_wait_supersedes_the_older_one() {
    let readiness = Arc::new(RegistrationReadiness::new(1));
    let (older, older_receipt) = receipt();
    let older_wait = readiness.wait(0, vec![older_receipt]);
    let (_newer, newer_receipt) = receipt();
    let mut newer_wait = Box::pin(readiness.wait(0, vec![newer_receipt]));

    older_wait.await;
    let _ = older.send(Ok(()));
    assert!(!readiness.is_ready(0), "a superseded wait sets nothing");
    assert!((&mut newer_wait).now_or_never().is_none());
}
