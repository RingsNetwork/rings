//! Browser mail reception over retained IndexedDB stores.
//!
//! Law: the offer is chosen before processor build; the recipient runs as a node task, and
//! closing the provider stops it. Inbox writes precede notifications and recipient records.
//! Opened verdicts belong to a delegatee key; filed Message-IDs remain shared across tabs.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use js_sys::Function;
use js_sys::Promise;
use js_sys::Uint8Array;
use rings_core::storage::idb::IdbStorage;
use rings_core::storage::KvStorageInterface;
use rings_core::storage::RecordAuthority;
use rings_derive::wasm_export;
use serde::Serialize;
use thiserror::Error;
use wasm_bindgen::JsError;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::future_to_promise;

use crate::mail::open::OpenedMail;
use crate::mail::recipient::KvInbox;
use crate::mail::recipient::MailInbox;
use crate::mail::recipient::Recipient;
use crate::mail::recipient::RecipientError;
use crate::mail::recipient::StoredMail;
use crate::mail::recipient::DEFAULT_MAIL_POLL_INTERVAL;
use crate::mail::registration::MailOffer;
use crate::processor::ProcessorConfig;
use crate::provider::Provider;

/// The row capacity of each browser mail store. Law: a full store refuses new rows and never
/// evicts an inbox message or a deduplication record; a failed poll retries on its schedule.
pub(crate) const MAIL_STORAGE_ROWS: u32 = 50_000;

/// A validated browser mail heartbeat. Constructing options does not enable an existing node;
/// pass them to [`Provider::new_provider_with_mail`] before it builds the processor.
#[derive(Default)]
#[wasm_export]
pub struct BrowserMailOptions {
    /// The validated schedule, defaulting to a 30-second heartbeat and 90-second freshness.
    offer: MailOffer,
}

#[wasm_export]
impl BrowserMailOptions {
    /// Choose heartbeat and freshness in milliseconds, using the default for an omitted value.
    /// Law: every supplied schedule passes [`MailOffer::new`] before any storage is opened.
    #[wasm_bindgen(constructor)]
    pub fn new(heartbeat_interval_ms: Option<u32>, ttl_ms: Option<u32>) -> Result<Self, JsError> {
        let default_offer = MailOffer::default();
        let heartbeat_interval = heartbeat_interval_ms
            .map_or(default_offer.heartbeat_interval(), |milliseconds| {
                Duration::from_millis(u64::from(milliseconds))
            });
        let ttl = ttl_ms.map_or(default_offer.ttl(), |milliseconds| {
            Duration::from_millis(u64::from(milliseconds))
        });
        Ok(Self {
            offer: MailOffer::new(heartbeat_interval, ttl).map_err(JsError::from)?,
        })
    }
}

/// Why browser mail construction or an inbox operation failed. JavaScript promises translate
/// these typed failures to `Error` at their boundary.
#[derive(Debug, Error)]
pub enum BrowserMailError {
    /// This provider was built without mail reception.
    #[error("mail reception was not enabled at construction")]
    Disabled,
    /// A node or named IndexedDB store could not be opened.
    #[error(transparent)]
    Provider(#[from] crate::error::Error),
    /// An inbox operation could not complete.
    #[error("mail inbox: {0}")]
    Storage(#[from] rings_core::error::Error),
    /// The recipient could not start or finish a poll.
    #[error(transparent)]
    Recipient(#[from] RecipientError<rings_core::error::Error>),
    /// The browser executor could not accept the node task.
    #[error(transparent)]
    Runtime(#[from] rings_runtime::RuntimeUnavailable),
    /// A stored mail could not be represented in JavaScript.
    #[error("cannot serialize browser mail: {0}")]
    Serialization(#[from] serde_wasm_bindgen::Error),
    /// An inbox row does not use the naming law of [`KvInbox`].
    #[error("invalid mail inbox record name: {0}")]
    RecordName(String),
}

/// The inbox and running recipient of one provider. Clones of the provider share this state,
/// so every inbox API uses the same retained store as its poll task.
pub(crate) struct BrowserMail {
    /// The retained inbox shared by API reads and the recipient's filing boundary.
    inbox: Rc<KvInbox<IdbStorage>>,
    /// The recipient shared by the task and test probes; its poll lock serializes their work.
    recipient: Rc<Recipient<NotifyingInbox, IdbStorage>>,
}

/// The filing boundary: commit to the retained inbox, then notify JavaScript of the mail id.
struct NotifyingInbox {
    /// The inbox shared with browser reads; sharing never duplicates the stored mail.
    inbox: Rc<KvInbox<IdbStorage>>,
    /// The synchronous callback installed before the first poll, if requested by the embedder.
    on_mail: Option<Function>,
}

#[async_trait(?Send)]
impl MailInbox for NotifyingInbox {
    type Error = rings_core::error::Error;

    /// Law: a notification implies a completed inbox write. A callback exception is logged;
    /// it cannot undo that write or prevent the recipient remembering its filed Message-ID.
    async fn file(&self, mail: &OpenedMail) -> Result<(), Self::Error> {
        self.inbox.file(mail).await?;
        if let Some(callback) = &self.on_mail {
            if let Err(error) =
                callback.call1(&JsValue::UNDEFINED, &mail.mail_id.to_string().into())
            {
                tracing::warn!(?error, "browser mail callback threw after filing");
            }
        }
        Ok(())
    }
}

/// A JavaScript inbox row. The raw RFC 5322 message is a `Uint8Array` and the epoch timestamp
/// is a `bigint`, preserving every stored byte and the full millisecond integer.
#[derive(Serialize)]
struct BrowserStoredMail {
    /// The ciphertext's mail digest, without the storage prefix.
    id: String,
    /// The complete raw RFC 5322 message.
    #[serde(with = "serde_wasm_bindgen::preserve")]
    message: Uint8Array,
    /// The filing time in milliseconds since the Unix epoch.
    filed_at_ms: u128,
}

impl BrowserStoredMail {
    /// Represent one stored message without decoding its RFC 5322 bytes as text.
    fn new(id: String, stored: StoredMail) -> Self {
        Self {
            id,
            message: Uint8Array::from(stored.message.as_slice()),
            filed_at_ms: stored.filed_at_ms,
        }
    }
}

/// Open a retained mail store under the provider's namespace. Law: failure never falls back
/// to volatile storage, and a successful open permits no capacity-based eviction.
async fn open_mail_storage(
    storage_name: &str,
    suffix: &str,
) -> Result<IdbStorage, BrowserMailError> {
    let name = format!("{storage_name}/{suffix}");
    IdbStorage::new_with_cap_name_and_authority(MAIL_STORAGE_ROWS, &name, RecordAuthority::Retained)
        .await
        .map_err(|source| crate::error::Error::BrowserStorageOpen { name, source }.into())
}

/// Serialize the browser API's rows as plain objects with exact large integers.
fn serialize_mail(value: &impl Serialize) -> Result<JsValue, BrowserMailError> {
    value
        .serialize(
            &serde_wasm_bindgen::Serializer::new().serialize_large_number_types_as_bigints(true),
        )
        .map_err(BrowserMailError::from)
}

impl Provider {
    /// Build a browser recipient over `storage_name/mail-inbox` and `storage_name/mail-records`.
    /// Law: both retained stores open before the descriptor is enabled, and the first poll and
    /// every later poll are node tasks stopped by [`Provider::close`].
    ///
    /// The callback receives each filed mail's id after its durable inbox write. It should
    /// return promptly; a thrown exception is logged. It may fire again if a crash or a failed
    /// record write interrupts filing before the recipient remembers it (the at-least-once law).
    pub async fn new_browser_provider_with_mail(
        config: ProcessorConfig,
        storage_name: String,
        options: BrowserMailOptions,
        on_mail: Option<Function>,
    ) -> Result<Self, BrowserMailError> {
        let inbox = Rc::new(KvInbox::new(
            open_mail_storage(&storage_name, "mail-inbox").await?,
        ));
        let records = open_mail_storage(&storage_name, "mail-records").await?;
        let mut provider =
            Self::new_browser_provider_with_mail_offer(config, storage_name, Some(options.offer))
                .await?;
        let installed = provider.install_browser_mail(inbox, records, on_mail);
        if let Err(error) = installed {
            if let Err(close_error) = provider.close().await {
                tracing::warn!(%close_error, "failed to close browser provider after mail setup");
            }
            return Err(error);
        }
        Ok(provider)
    }

    /// Install a single recipient over the opened stores and spawn its run loop. Law: every
    /// fallible setup step finishes before either the state or the task becomes observable.
    fn install_browser_mail(
        &mut self,
        inbox: Rc<KvInbox<IdbStorage>>,
        records: IdbStorage,
        on_mail: Option<Function>,
    ) -> Result<(), BrowserMailError> {
        let tasks = self.processor.tasks();
        let spawner = tasks.spawner()?;
        let recipient = Rc::new(Recipient::start(
            Arc::clone(&self.processor),
            NotifyingInbox {
                inbox: Rc::clone(&inbox),
                on_mail,
            },
            records,
            DEFAULT_MAIL_POLL_INTERVAL,
        )?);
        let stop = tasks.stop_token();
        let mail = Rc::new(BrowserMail { inbox, recipient });
        let recipient = Rc::clone(&mail.recipient);
        self.browser_mail = Some(mail);
        spawner.spawn(async move { recipient.run(stop).await });
        Ok(())
    }

    /// Borrow the enabled mail state, rejecting calls on a provider that did not opt in.
    fn enabled_browser_mail(&self) -> Result<Rc<BrowserMail>, BrowserMailError> {
        self.browser_mail
            .as_ref()
            .map(Rc::clone)
            .ok_or(BrowserMailError::Disabled)
    }
}

#[wasm_export]
impl Provider {
    /// Create a mail-enabled provider. Existing constructors keep mail disabled. Call `listen()`
    /// to publish the descriptor; `options` defaults to a 30-second heartbeat and 90-second TTL.
    /// `on_mail(id)` reports a completed inbox write without polling IndexedDB. A crash or a
    /// failed record write can repeat that id on retry; callback handlers should be idempotent.
    ///
    /// One-key law: a browser takes a new delegatee key at each load. Mail accepted under the
    /// previous key around a reload never opens, even if persisted; the default freshness
    /// window plus clock skew is at most 93 seconds (a custom TTL adds 3 seconds of skew).
    /// Already filed inbox mail and deduplication records survive closing and reopening.
    pub fn new_provider_with_mail(
        config: ProcessorConfig,
        storage_name: String,
        options: BrowserMailOptions,
        on_mail: Option<Function>,
    ) -> Promise {
        future_to_promise(async move {
            let provider =
                Self::new_browser_provider_with_mail(config, storage_name, options, on_mail)
                    .await
                    .map_err(JsError::from)?;
            Ok(JsValue::from(provider))
        })
    }

    /// List filed mail as `{ id: string, message: Uint8Array, filed_at_ms: bigint }` objects.
    /// Law: listing does not retire mail or its deduplication records; mail-off providers reject.
    pub fn list_mail(&self) -> Promise {
        let mail = self.enabled_browser_mail();
        future_to_promise(async move {
            let mail = mail.map_err(JsError::from)?;
            let stored: Vec<(String, StoredMail)> =
                mail.inbox.store().get_all().await.map_err(JsError::from)?;
            let rows = stored
                .into_iter()
                .map(|(name, stored)| {
                    let id = name
                        .strip_prefix("mail-")
                        .ok_or_else(|| BrowserMailError::RecordName(name.clone()))?;
                    Ok(BrowserStoredMail::new(id.to_owned(), stored))
                })
                .collect::<Result<Vec<_>, BrowserMailError>>()
                .map_err(JsError::from)?;
            serialize_mail(&rows).map_err(|error| JsError::from(error).into())
        })
    }

    /// Read one filed mail by its id, returning the same object as `list_mail`, or `null`.
    pub fn read_mail(&self, id: String) -> Promise {
        let mail = self.enabled_browser_mail();
        future_to_promise(async move {
            let mail = mail.map_err(JsError::from)?;
            let stored: Option<StoredMail> = mail
                .inbox
                .store()
                .get(&format!("mail-{id}"))
                .await
                .map_err(JsError::from)?;
            match stored {
                Some(stored) => serialize_mail(&BrowserStoredMail::new(id, stored))
                    .map_err(|error| JsError::from(error).into()),
                None => Ok(JsValue::NULL),
            }
        })
    }

    /// Remove one inbox mail by id; a missing id succeeds. Law: its recipient records remain,
    /// so a resend within retention cannot restore a message the embedder deliberately removed.
    pub fn remove_mail(&self, id: String) -> Promise {
        let mail = self.enabled_browser_mail();
        future_to_promise(async move {
            let mail = mail.map_err(JsError::from)?;
            <IdbStorage as KvStorageInterface<StoredMail>>::remove(
                mail.inbox.store(),
                &format!("mail-{id}"),
            )
            .await
            .map_err(JsError::from)?;
            Ok(JsValue::UNDEFINED)
        })
    }
}

#[cfg(test)]
impl Provider {
    /// Share the processor with browser gateway tests without changing the JavaScript API.
    pub(crate) fn mail_processor_for_test(&self) -> Arc<crate::processor::Processor> {
        Arc::clone(&self.processor)
    }

    /// Poll the same recipient as the node task; its poll lock serializes both paths.
    pub(crate) async fn poll_mail_for_test(
        &self,
    ) -> Result<crate::mail::recipient::PollReport, BrowserMailError> {
        Ok(self.enabled_browser_mail()?.recipient.poll().await?)
    }
}
