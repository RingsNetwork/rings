//! Browser mail reception through retained IndexedDB stores and the exported JavaScript API.
//!
//! Descriptor publication uses two connected browser peers and awaits their registration
//! readiness events. The other tests use a singleton overlay, whose local placements still
//! produce owner store reports. No test needs an external STUN server or a second browser
//! process. Two providers sharing one storage name model tabs holding distinct delegatee keys
//! for the same DID.

use std::collections::BTreeMap;

use futures::channel::mpsc;
use futures::FutureExt;
use futures::StreamExt;
use js_sys::Array;
use js_sys::Function;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_core::delegation::DelegateeKey;
use rings_core::ecc::SecretKey;
use rings_core::storage::idb::IdbStorage;
use rings_core::storage::KvStorageInterface;
use rings_core::storage::RecordAuthority;
use rings_core::utils::js_value;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::wasm_bindgen_test;

use super::provider_did;
use super::with_hang_guard;
use super::TEST_HANG_GUARD;
use crate::mail::descriptor::MailRecipient;
use crate::mail::gateway::MailDrop;
use crate::mail::gateway::MailGateway;
use crate::mail::recipient::KvInbox;
use crate::mail::recipient::Recipient;
use crate::mail::recipient::RecipientError;
use crate::mail::recipient::RecipientRecord;
use crate::mail::recipient::StoredMail;
use crate::mail::recipient::DEFAULT_MAIL_POLL_INTERVAL;
use crate::processor::ProcessorConfig;
use crate::provider::browser::provider::mail::MAIL_STORAGE_ROWS;
use crate::provider::browser::BrowserMailOptions;
use crate::provider::browser::ProviderListener;
use crate::provider::Provider;
use crate::tests::TEST_ICE_SERVERS;

/// A raw message whose stable Message-ID identifies every randomized resend in these tests.
const MESSAGE: &[u8] = b"Message-ID: <browser-mail@example.org>\r\n\r\nBrowser inbox.\r\n";

/// A different message whose deduplication key requires another inbox row.
const SECOND_MESSAGE: &[u8] =
    b"Message-ID: <browser-mail-second@example.org>\r\n\r\nNext mail.\r\n";

/// A recipient whose inbox and records have independently bounded retained IndexedDB stores.
type RetainedRecipient = Recipient<KvInbox<IdbStorage>, IdbStorage>;

/// A JavaScript callback and its buffered notifications; no notification needs an inbox scan.
struct MailNotifications {
    /// The Rust closure kept alive for the duration of its provider's callback registration.
    callback: Closure<dyn Fn(String)>,
    /// The ordered identifiers delivered by the callback.
    received: mpsc::UnboundedReceiver<String>,
}

impl MailNotifications {
    /// Install the notification channel before a provider starts its first poll.
    fn new() -> Self {
        let (sender, received) = mpsc::unbounded();
        let callback = Closure::new(move |identifier: String| {
            sender.unbounded_send(identifier).unwrap();
        });
        Self { callback, received }
    }

    /// The JavaScript function handle for this log, sharing the same live closure.
    fn function(&self) -> Function {
        self.callback.as_ref().unchecked_ref::<Function>().clone()
    }

    /// Await one filed identifier, including one delivered before this future was created.
    async fn next(&mut self) -> String {
        self.received.next().await.unwrap()
    }

    /// Assert that completed polls have produced no unconsumed notification.
    fn assert_empty(&mut self) {
        assert!(self.received.next().now_or_never().is_none());
    }
}

/// A browser configuration with a fresh delegatee key for the account `identity`.
fn configuration(identity: &SecretKey) -> ProcessorConfig {
    let delegatee_key = DelegateeKey::new_with_seckey(identity).unwrap();
    ProcessorConfig::new(0, TEST_ICE_SERVERS.to_owned(), delegatee_key, 200).dht_virtual_nodes(0)
}

/// A storage namespace isolated from every other browser test run.
fn storage_name() -> String {
    format!("rings-browser-mail-test-{}", uuid::Uuid::new_v4())
}

/// The recipient named by a configuration, used to preseed mail before receiving is enabled.
fn recipient_of(config: &ProcessorConfig) -> MailRecipient {
    let delegatee_key = config.delegatee_key();
    MailRecipient {
        did: delegatee_key.delegator_did(),
        encryption_key: delegatee_key.delegatee_public_key(),
    }
}

/// A gateway using the provider's overlay and its ordinary reported-write path.
fn gateway(provider: &Provider) -> MailGateway {
    MailGateway::new(provider.mail_processor_for_test())
}

/// Connect a listening provider to a second browser peer and await both registration events.
/// Law: readiness requires a publish that began with an admitted edge, so a singleton cannot
/// witness it. The first admission wakes each listener's early publish, and owner reports
/// resolve readiness before the caller performs its one descriptor lookup.
async fn connect_registration_peer(provider: &Provider) -> (Provider, ProviderListener) {
    let peer = Provider::new_browser_provider_with_storage(
        configuration(&SecretKey::random()),
        storage_name(),
    )
    .await
    .unwrap();
    let peer_listener = peer.listen();
    JsFuture::from(peer_listener.started()).await.unwrap();
    let processor = provider.mail_processor_for_test();
    let peer_processor = peer.mail_processor_for_test();
    let offer = processor
        .swarm
        .create_offer(peer_processor.did())
        .await
        .unwrap();
    let answer = peer_processor
        .swarm
        .answer_offer(offer)
        .await
        .unwrap()
        .answer()
        .expect("the peer holds no competing offer, so it answers");
    processor.swarm.accept_answer(answer).await.unwrap();
    provider.registrations_published().await;
    peer.registrations_published().await;
    (peer, peer_listener)
}

/// Read the inbox through the exported promise API, preserving each returned JavaScript row.
async fn listed_mail(provider: &Provider) -> Vec<JsValue> {
    let value = JsFuture::from(provider.list_mail()).await.unwrap();
    assert!(Array::is_array(&value));
    Array::from(&value).iter().collect()
}

/// Read one named property from a row returned to JavaScript.
fn property(value: &JsValue, name: &str) -> JsValue {
    Reflect::get(value, &JsValue::from_str(name)).unwrap()
}

/// Assert that a JavaScript row preserves the mail id, raw RFC 5322 bytes and filing time.
fn assert_message(row: &JsValue, identifier: &str) {
    assert_eq!(property(row, "id").as_string().unwrap(), identifier);
    let message = property(row, "message").dyn_into::<Uint8Array>().unwrap();
    assert_eq!(message.to_vec(), MESSAGE);
    let filed_at_ms = property(row, "filed_at_ms");
    assert!(filed_at_ms.is_bigint());
    assert!(js_value::deserialize::<u128>(filed_at_ms).unwrap() > 0);
}

/// Snapshot retained records under the same row budget and authority as the provider.
async fn recipient_records(storage_name: &str) -> BTreeMap<String, RecipientRecord> {
    let store = IdbStorage::new_with_cap_name_and_authority(
        MAIL_STORAGE_ROWS,
        &format!("{storage_name}/mail-records"),
        RecordAuthority::Retained,
    )
    .await
    .unwrap();
    let records: Vec<(String, RecipientRecord)> = store.get_all().await.unwrap();
    records.into_iter().collect()
}

/// Close the node while its listener and mail poller are live; both must finish before return.
async fn close_listening_provider(provider: Provider, listener: ProviderListener) {
    provider.close().await.unwrap();
    JsFuture::from(listener.task()).await.unwrap();
}

/// Law: only a valid custom heartbeat schedule can cross the JavaScript options boundary.
#[wasm_bindgen_test]
fn test_mail_options_validate_the_heartbeat_schedule() {
    assert!(BrowserMailOptions::new(None, None).is_ok());
    assert!(BrowserMailOptions::new(Some(10_000), Some(40_000)).is_ok());
    assert!(BrowserMailOptions::new(Some(90_000), Some(30_000)).is_err());
    assert!(BrowserMailOptions::new(Some(300_000), Some(600_000)).is_err());
}

/// Law: construction opts into the listener's descriptor heartbeat; existing constructors do not.
#[wasm_bindgen_test]
async fn test_mail_descriptor_is_published_only_when_enabled() {
    with_hang_guard(
        "test_mail_descriptor_is_published_only_when_enabled",
        TEST_HANG_GUARD,
        async {
            let config = configuration(&SecretKey::random());
            let provider = Provider::new_browser_provider_with_storage(config, storage_name())
                .await
                .unwrap();
            let listener = provider.listen();
            JsFuture::from(listener.started()).await.unwrap();
            let (peer, peer_listener) = connect_registration_peer(&provider).await;
            assert_eq!(
                gateway(&peer)
                    .recipient(provider_did(&provider))
                    .await
                    .unwrap(),
                None
            );
            close_listening_provider(provider, listener).await;
            close_listening_provider(peer, peer_listener).await;

            let config = configuration(&SecretKey::random());
            let expected = recipient_of(&config);
            let provider = Provider::new_browser_provider_with_mail(
                config,
                storage_name(),
                BrowserMailOptions::default(),
                None,
            )
            .await
            .unwrap();
            let listener = provider.listen();
            JsFuture::from(listener.started()).await.unwrap();
            let (peer, peer_listener) = connect_registration_peer(&provider).await;
            assert_eq!(
                gateway(&peer).recipient(expected.did).await.unwrap(),
                Some(expected)
            );
            close_listening_provider(provider, listener).await;
            close_listening_provider(peer, peer_listener).await;
        },
    )
    .await;
}

/// Seed a reported gateway delivery while the inbox is disabled, so only the enabled
/// provider's automatic startup poll can produce the first notification.
async fn seed_pending_mail(config: ProcessorConfig, storage_name: String) -> String {
    let recipient = recipient_of(&config);
    let provider = Provider::new_browser_provider_with_storage(config, storage_name)
        .await
        .unwrap();
    let identifier = gateway(&provider)
        .deliver(&recipient, MESSAGE)
        .await
        .unwrap()
        .to_string();
    provider.close().await.unwrap();
    identifier
}

/// Assert JavaScript list and read expose exactly the one expected inbox entry.
async fn assert_single_message(provider: &Provider, identifier: &str) {
    let listed = listed_mail(provider).await;
    assert_eq!(listed.len(), 1);
    assert_message(listed.first().unwrap(), identifier);
    let read = JsFuture::from(provider.read_mail(identifier.to_owned()))
        .await
        .unwrap();
    assert_message(&read, identifier);
}

/// Law: automatic reception notifies after filing, and both the inbox and deduplication
/// records survive a close and a reload with a new delegatee key.
#[wasm_bindgen_test]
async fn test_mail_startup_notification_restart_deduplication_and_removal() {
    with_hang_guard(
        "test_mail_startup_notification_restart_deduplication_and_removal",
        TEST_HANG_GUARD,
        async {
            let identity = SecretKey::random();
            let config = configuration(&identity);
            let storage_name = storage_name();
            let identifier = seed_pending_mail(config.clone(), storage_name.clone()).await;
            let mut notifications = MailNotifications::new();
            let provider = Provider::new_browser_provider_with_mail(
                config,
                storage_name.clone(),
                BrowserMailOptions::default(),
                Some(notifications.function()),
            )
            .await
            .unwrap();
            assert_eq!(notifications.next().await, identifier);
            provider.poll_mail_for_test().await.unwrap();
            assert_single_message(&provider, &identifier).await;
            notifications.assert_empty();
            let before = recipient_records(&storage_name).await;
            assert!(before
                .values()
                .any(|record| matches!(record, RecipientRecord::Filed { .. })));
            assert!(before
                .values()
                .any(|record| matches!(record, RecipientRecord::Opened { .. })));
            provider.close().await.unwrap();

            let reloaded_config = configuration(&identity);
            let current_recipient = recipient_of(&reloaded_config);
            let provider = Provider::new_browser_provider_with_mail(
                reloaded_config,
                storage_name.clone(),
                BrowserMailOptions::default(),
                Some(notifications.function()),
            )
            .await
            .unwrap();
            provider.poll_mail_for_test().await.unwrap();
            assert_single_message(&provider, &identifier).await;
            let after = recipient_records(&storage_name).await;
            assert!(before
                .iter()
                .all(|(name, record)| after.get(name) == Some(record)));
            gateway(&provider)
                .deliver(&current_recipient, MESSAGE)
                .await
                .unwrap();
            assert_eq!(provider.poll_mail_for_test().await.unwrap().duplicates, 1);
            assert_single_message(&provider, &identifier).await;
            notifications.assert_empty();

            JsFuture::from(provider.remove_mail(identifier.clone()))
                .await
                .unwrap();
            assert!(listed_mail(&provider).await.is_empty());
            assert!(JsFuture::from(provider.read_mail(identifier))
                .await
                .unwrap()
                .is_null());
            gateway(&provider)
                .deliver(&current_recipient, MESSAGE)
                .await
                .unwrap();
            assert_eq!(provider.poll_mail_for_test().await.unwrap().duplicates, 1);
            assert!(listed_mail(&provider).await.is_empty());
            notifications.assert_empty();
            provider.close().await.unwrap();
        },
    )
    .await;
}

/// Law: a tab's unreadable verdict cannot hide mail from the tab holding its key, while a
/// filed Message-ID deduplicates resends under either key in the shared stores.
#[wasm_bindgen_test]
async fn test_mail_two_tabs_scope_unreadable_records_and_share_filed_records() {
    with_hang_guard(
        "test_mail_two_tabs_scope_unreadable_records_and_share_filed_records",
        TEST_HANG_GUARD,
        async {
            let identity = SecretKey::random();
            let first_config = configuration(&identity);
            let second_config = configuration(&identity);
            let first_recipient = recipient_of(&first_config);
            let second_recipient = recipient_of(&second_config);
            assert_eq!(first_recipient.did, second_recipient.did);
            assert_ne!(
                first_recipient.encryption_key,
                second_recipient.encryption_key
            );
            let storage_name = storage_name();
            let mut first_notifications = MailNotifications::new();
            let first = Provider::new_browser_provider_with_mail(
                first_config,
                storage_name.clone(),
                BrowserMailOptions::default(),
                Some(first_notifications.function()),
            )
            .await
            .unwrap();
            first.poll_mail_for_test().await.unwrap();
            let identifier = gateway(&first)
                .deliver(&second_recipient, MESSAGE)
                .await
                .unwrap()
                .to_string();
            assert_eq!(first.poll_mail_for_test().await.unwrap().unreadable, 1);
            assert!(listed_mail(&first).await.is_empty());

            let mut second_notifications = MailNotifications::new();
            let second = Provider::new_browser_provider_with_mail(
                second_config,
                storage_name,
                BrowserMailOptions::default(),
                Some(second_notifications.function()),
            )
            .await
            .unwrap();
            assert_eq!(second_notifications.next().await, identifier);
            second.poll_mail_for_test().await.unwrap();
            assert_single_message(&first, &identifier).await;
            assert_single_message(&second, &identifier).await;
            gateway(&first)
                .deliver(&first_recipient, MESSAGE)
                .await
                .unwrap();
            assert_eq!(first.poll_mail_for_test().await.unwrap().duplicates, 1);
            assert_single_message(&first, &identifier).await;
            first_notifications.assert_empty();
            second_notifications.assert_empty();
            first.close().await.unwrap();
            second.close().await.unwrap();
        },
    )
    .await;
}

/// Law: an embedder callback that throws cannot prevent a filed mail from being remembered.
#[wasm_bindgen_test]
async fn test_mail_throwing_callback_preserves_filing_and_deduplication() {
    with_hang_guard(
        "test_mail_throwing_callback_preserves_filing_and_deduplication",
        TEST_HANG_GUARD,
        async {
            let config = configuration(&SecretKey::random());
            let recipient = recipient_of(&config);
            let storage_name = storage_name();
            let identifier = seed_pending_mail(config.clone(), storage_name.clone()).await;
            let provider = Provider::new_browser_provider_with_mail(
                config,
                storage_name,
                BrowserMailOptions::default(),
                Some(Function::new_no_args(
                    "throw new Error('mail callback failed');",
                )),
            )
            .await
            .unwrap();
            provider.poll_mail_for_test().await.unwrap();
            assert_single_message(&provider, &identifier).await;
            gateway(&provider)
                .deliver(&recipient, MESSAGE)
                .await
                .unwrap();
            assert_eq!(provider.poll_mail_for_test().await.unwrap().duplicates, 1);
            assert_single_message(&provider, &identifier).await;
            provider.close().await.unwrap();
        },
    )
    .await;
}

/// Open a recipient with small explicit budgets, exercising the provider's retained-storage
/// law without filling its production capacity with thousands of messages.
async fn bounded_recipient(
    provider: &Provider,
    storage_name: &str,
    inbox_rows: u32,
    record_rows: u32,
) -> RetainedRecipient {
    let inbox = IdbStorage::new_with_cap_name_and_authority(
        inbox_rows,
        &format!("{storage_name}/mail-inbox"),
        RecordAuthority::Retained,
    )
    .await
    .unwrap();
    let records = IdbStorage::new_with_cap_name_and_authority(
        record_rows,
        &format!("{storage_name}/mail-records"),
        RecordAuthority::Retained,
    )
    .await
    .unwrap();
    Recipient::start(
        provider.mail_processor_for_test(),
        KvInbox::new(inbox),
        records,
        DEFAULT_MAIL_POLL_INTERVAL,
    )
    .unwrap()
}

/// Snapshot every filed message, including its timestamp, so a rewrite is observable.
async fn retained_messages(recipient: &RetainedRecipient) -> BTreeMap<String, StoredMail> {
    recipient
        .inbox()
        .store()
        .get_all()
        .await
        .unwrap()
        .into_iter()
        .collect()
}

/// Law: a full inbox fails its poll without eviction, and the pending mail is retried once
/// the embedder removes a filed message to make room.
#[wasm_bindgen_test]
async fn test_mail_full_inbox_preserves_filed_mail_and_retries_pending_mail() {
    with_hang_guard(
        "test_mail_full_inbox_preserves_filed_mail_and_retries_pending_mail",
        TEST_HANG_GUARD,
        async {
            let config = configuration(&SecretKey::random());
            let target = recipient_of(&config);
            let storage_name = storage_name();
            let provider =
                Provider::new_browser_provider_with_storage(config, storage_name.clone())
                    .await
                    .unwrap();
            let recipient = bounded_recipient(&provider, &storage_name, 1, 8).await;
            let first_identifier = gateway(&provider).deliver(&target, MESSAGE).await.unwrap();
            assert_eq!(recipient.poll().await.unwrap().filed, 1);
            let before = retained_messages(&recipient).await;
            gateway(&provider)
                .deliver(&target, SECOND_MESSAGE)
                .await
                .unwrap();
            assert!(matches!(
                recipient.poll().await,
                Err(RecipientError::Inbox(
                    rings_core::error::Error::StorageBudgetExhausted { .. }
                ))
            ));
            assert_eq!(retained_messages(&recipient).await, before);
            <IdbStorage as KvStorageInterface<StoredMail>>::remove(
                recipient.inbox().store(),
                &format!("mail-{first_identifier}"),
            )
            .await
            .unwrap();
            assert_eq!(recipient.poll().await.unwrap().filed, 1);
            let after = retained_messages(&recipient).await;
            assert_eq!(after.len(), 1);
            assert_eq!(after.values().next().unwrap().message, SECOND_MESSAGE);
            drop(recipient);
            provider.close().await.unwrap();
        },
    )
    .await;
}

/// Law: a full records store fails the poll after filing without evicting its filed
/// Message-ID; repeated polls preserve the inbox row instead of filing it again.
#[wasm_bindgen_test]
async fn test_mail_full_records_preserves_filed_mail_and_deduplication() {
    with_hang_guard(
        "test_mail_full_records_preserves_filed_mail_and_deduplication",
        TEST_HANG_GUARD,
        async {
            let config = configuration(&SecretKey::random());
            let target = recipient_of(&config);
            let storage_name = storage_name();
            let provider =
                Provider::new_browser_provider_with_storage(config, storage_name.clone())
                    .await
                    .unwrap();
            let recipient = bounded_recipient(&provider, &storage_name, 4, 1).await;
            gateway(&provider).deliver(&target, MESSAGE).await.unwrap();
            assert!(matches!(
                recipient.poll().await,
                Err(RecipientError::Records(
                    rings_core::error::Error::StorageBudgetExhausted { .. }
                ))
            ));
            let before = retained_messages(&recipient).await;
            assert_eq!(before.len(), 1);
            for _ in 0..2 {
                assert!(matches!(
                    recipient.poll().await,
                    Err(RecipientError::Records(
                        rings_core::error::Error::StorageBudgetExhausted { .. }
                    ))
                ));
                assert_eq!(retained_messages(&recipient).await, before);
            }
            let records = recipient_records(&storage_name).await;
            assert_eq!(records.len(), 1);
            assert!(matches!(
                records.values().next(),
                Some(RecipientRecord::Filed { .. })
            ));
            drop(recipient);
            provider.close().await.unwrap();
        },
    )
    .await;
}
