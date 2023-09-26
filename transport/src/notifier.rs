//! This module contains the [Notifier] struct.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;

use crate::error::Error;
use crate::error::Result;

#[derive(Default)]
struct NotifierState {
    /// Indicates whether state has succeeded.
    pub(crate) succeeded: Option<bool>,

    /// The wakers associated with State.
    pub(crate) wakers: Vec<std::task::Waker>,
}

/// Notifier allow to wait for a result.
/// It does not provide timeout mechanism for platform compatibility.
/// It is recommended to use tokio::time::timeout instead in native-webrtc feature.
/// It is recommended to use set_timeout in web-sys-webrtc feature.
#[derive(Clone, Default)]
pub struct Notifier(Arc<Mutex<NotifierState>>);

impl Notifier {
    /// Set the result of the notifier. Will also wake all the wakers.
    pub fn set_result(&self, succeeded: bool) {
        let mut state = self.0.lock().unwrap();
        state.succeeded = Some(succeeded);
        for waker in state.wakers.drain(..) {
            waker.wake();
        }
    }
}

impl Future for Notifier {
    type Output = Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.0.lock().unwrap();

        match *state {
            NotifierState {
                succeeded: Some(true),
                ..
            } => Poll::Ready(Ok(())),

            NotifierState {
                succeeded: Some(false),
                ..
            } => Poll::Ready(Err(Error::NotifierFailed)),

            _ => {
                state.wakers.push(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_notifier_success() {
        let notifier = Notifier::default();

        let notifier_clone = notifier.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            notifier_clone.set_result(true);
        });

        let mut jobs = vec![];

        // Await three times to ensure that the notifier will always return result.
        for _ in 0..3 {
            let notifier_clone = notifier.clone();
            jobs.push(tokio::spawn(async move {
                notifier_clone.await.unwrap();
            }));
        }

        // Await three times after wake to ensure that the notifier will always return result.
        for _ in 0..3 {
            let notifier_clone = notifier.clone();
            jobs.push(tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                notifier_clone.await.unwrap();
            }));
        }

        futures::future::join_all(jobs).await;
    }

    #[tokio::test]
    #[should_panic(expected = "NotifierFailed")]
    async fn test_notifier_fail() {
        let notifier = Notifier::default();

        let notifier_clone = notifier.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            notifier_clone.set_result(false);
        });

        let mut jobs = vec![];

        // Await three times to ensure that the notifier will always return result.
        for _ in 0..3 {
            let notifier_clone = notifier.clone();
            jobs.push(tokio::spawn(async move {
                assert!(notifier_clone.await.is_err());
            }));
        }

        // Await three times after wake to ensure that the notifier will always return result.
        for _ in 0..3 {
            let notifier_clone = notifier.clone();
            jobs.push(tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                assert!(notifier_clone.await.is_err());
            }));
        }

        futures::future::join_all(jobs).await;
        notifier.await.unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "NotifierTimeout(1000)")]
    async fn test_notifier_timeout() {
        let notifier = Notifier::default();
        let ttl = 1000u64;

        tokio::time::timeout(tokio::time::Duration::from_millis(ttl), notifier)
            .await
            .map_err(|_| Error::NotifierTimeout(ttl))
            .unwrap()
            .unwrap();
    }
}
