//! Native witnesses of the recipient's key-isolation and cross-key deduplication laws.
//!
//! Two singleton processors hold distinct delegatee keys for the same DID. Reusing the
//! recipient's stores between their polls models the observation order of two tabs sharing
//! IndexedDB without making duplicate-DID routing part of the record law.

use std::convert::Infallible;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use futures::channel::oneshot;
use futures::lock::Mutex;
use futures::FutureExt;
use rand::rngs::OsRng;
use rings_core::consts::MAX_TTL_MS;
use rings_core::ecc::SecretKey;
use rings_core::storage::KvStorageInterface;
use rings_core::storage::MemStorage;
use rings_core::swarm::storage_api::ChordStorageInterface;

use super::KvInbox;
use super::MailInbox;
use super::PollReport;
use super::Recipient;
use super::RecipientRecord;
use super::StoredMail;
use super::DEFAULT_MAIL_POLL_INTERVAL;
use crate::mail::descriptor::MailRecipient;
use crate::mail::envelope::seal;
use crate::mail::envelope::SealedMail;
use crate::mail::mail_topic;
use crate::mail::open::OpenedMail;
use crate::processor::Processor;
use crate::tests::test_processor_builder;
use crate::tests::test_processor_config;

/// A message whose Message-ID must stay deduplicated across delegatee keys.
const MESSAGE: &[u8] = b"Message-ID: <shared@example.org>\r\n\r\nShared inbox.\r\n";

/// The retained state moved between processors to model their shared stores.
type MemoryRecipient = Recipient<KvInbox<MemStorage<StoredMail>>, MemStorage<RecipientRecord>>;

/// A singleton processor with a new delegatee key for `identity_key` and no storage replicas.
fn processor_for(identity_key: &SecretKey) -> Arc<Processor> {
    Arc::new(
        test_processor_builder(&test_processor_config(identity_key, 0))
            .dht_virtual_nodes(0)
            .build()
            .unwrap(),
    )
}

/// A recipient whose initial inbox and opening and filing records are empty.
fn memory_recipient(processor: Arc<Processor>) -> MemoryRecipient {
    Recipient::start(
        processor,
        KvInbox::new(MemStorage::new()),
        MemStorage::new(),
        DEFAULT_MAIL_POLL_INTERVAL,
    )
    .unwrap()
}

/// Seal the fixture message to exactly `processor`'s current delegatee key.
fn sealed_for(processor: &Processor) -> SealedMail {
    let target = MailRecipient {
        did: processor.did(),
        encryption_key: processor.delegatee_key().delegatee_public_key(),
    };
    seal(MESSAGE, &target, &mut OsRng).unwrap()
}

/// Make `processor` observe a live mail on its topic before the next recipient poll.
async fn store_mail(processor: &Processor, sealed: &SealedMail) {
    processor
        .swarm
        .storage_extend(
            &mail_topic(processor.did()),
            sealed.elements.clone(),
            MAX_TTL_MS,
        )
        .await
        .unwrap();
}

/// Law: an unreadable verdict belongs only to its opening key, while a filed Message-ID
/// remains shared even when its resend is sealed to a different key.
#[tokio::test]
async fn test_one_keys_unreadable_verdict_cannot_hide_mail_from_another_key() {
    let identity_key = SecretKey::random();
    let first_processor = processor_for(&identity_key);
    let second_processor = processor_for(&identity_key);
    assert_eq!(first_processor.did(), second_processor.did());
    assert_ne!(
        first_processor.delegatee_key().delegatee_public_key(),
        second_processor.delegatee_key().delegatee_public_key()
    );
    let sealed = sealed_for(&second_processor);
    store_mail(&first_processor, &sealed).await;
    store_mail(&second_processor, &sealed).await;

    let first_recipient = memory_recipient(Arc::clone(&first_processor));
    assert_eq!(first_recipient.poll().await.unwrap(), PollReport {
        unreadable: 1,
        ..PollReport::default()
    });
    assert_eq!(
        first_recipient.poll().await.unwrap(),
        PollReport::default(),
        "a key does not repeat its unreadable verdict"
    );

    let second_recipient = Recipient {
        processor: second_processor,
        ..first_recipient
    };
    assert_eq!(second_recipient.poll().await.unwrap(), PollReport {
        filed: 1,
        ..PollReport::default()
    });
    let filed = second_recipient.inbox().store().get_all().await.unwrap();
    assert_eq!(filed.len(), 1);
    assert_eq!(filed.first().unwrap().1.message, MESSAGE);

    let resend = sealed_for(&first_processor);
    store_mail(&first_processor, &resend).await;
    let first_recipient = Recipient {
        processor: first_processor,
        ..second_recipient
    };
    assert_eq!(first_recipient.poll().await.unwrap(), PollReport {
        duplicates: 1,
        ..PollReport::default()
    });
    assert_eq!(first_recipient.inbox().store().count().await.unwrap(), 1);
}

/// An inbox that holds its first file operation until the test releases it.
struct PausedInbox {
    /// The number of file attempts, including an attempt still waiting for release.
    file_attempts: AtomicUsize,
    /// The one pending release; later attempts proceed without waiting.
    release_first_file: Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait]
impl MailInbox for PausedInbox {
    type Error = Infallible;

    /// Count this attempt and pause the first one before its filing record can be written.
    async fn file(&self, _: &OpenedMail) -> Result<(), Self::Error> {
        self.file_attempts.fetch_add(1, Ordering::SeqCst);
        let release = self.release_first_file.lock().await.take();
        if let Some(release) = release {
            release.await.unwrap();
        }
        Ok(())
    }
}

/// Law: a second poll on one recipient cannot file from a stale snapshot while the first
/// poll is still filing the same message.
#[tokio::test]
async fn test_concurrent_polls_of_one_recipient_file_once() {
    let processor = processor_for(&SecretKey::random());
    let sealed = sealed_for(&processor);
    store_mail(&processor, &sealed).await;
    let (release, waiting) = oneshot::channel();
    let recipient = Recipient::start(
        processor,
        PausedInbox {
            file_attempts: AtomicUsize::new(0),
            release_first_file: Mutex::new(Some(waiting)),
        },
        MemStorage::new(),
        DEFAULT_MAIL_POLL_INTERVAL,
    )
    .unwrap();
    let first_poll = recipient.poll();
    let second_poll = recipient.poll();
    futures::pin_mut!(first_poll, second_poll);
    assert!(first_poll.as_mut().now_or_never().is_none());
    assert!(second_poll.as_mut().now_or_never().is_none());
    assert_eq!(
        recipient.inbox().file_attempts.load(Ordering::SeqCst),
        1,
        "the second poll must wait before reading a filing snapshot"
    );

    release.send(()).unwrap();
    let (first_report, second_report) = futures::join!(first_poll, second_poll);
    assert_eq!(first_report.unwrap().filed, 1);
    assert_eq!(second_report.unwrap(), PollReport::default());
    assert_eq!(recipient.inbox().file_attempts.load(Ordering::SeqCst), 1);
}
