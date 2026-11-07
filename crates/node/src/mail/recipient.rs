//! The recipient's side: its reads of its mail topic, and what it remembers. It builds for native and browser nodes alike: its records and its inbox are any
//! key-value store (a file store natively, IndexedDB in a browser) and any [`MailInbox`].
//!
//! ```text
//!   poll(t):  E = fetch(mail_topic(d))                      the elements live at t
//!             R = records ∖ { r | outlived(r, t) }           retention
//!             for o ∈ open(E, d, k, fresh = ¬opened_R(k)):  k = the node's delegatee key
//!               Incomplete             ─▶ nothing           tried again on the next poll
//!               Unreadable             ─▶ opened(k, id)
//!               Opened(m) ∧ filed_R(h) ─▶ opened(k, id)      h = H(Message-ID), or id without one
//!               Opened(m)              ─▶ inbox.file(m); filed(h); opened(k, id)
//! ```
//!
//! Laws:
//! - **Schedule.** The recipient polls when it starts and every `I ≤ H / 2` while it runs
//!   ([`MAX_MAIL_POLL_INTERVAL`]): a mail written just after one poll lives `H` and the next
//!   poll comes within `H / 2`, which leaves the other half for clock skew and propagation.
//! - **No removal.** It never writes its mail topic: every element retires at its horizon, and
//!   a data carrier's remove budget (`DATA_TOMBSTONE_MAX_LEN`) is never spent on mail.
//! - **At least once.** A mail is filed before it is remembered, so a crash between the two
//!   files it again after a restart: a crash never loses a mail, at worst it is filed twice.
//! - **Deduplication.** Encryption is randomized, so a resent mail is a new ciphertext: mail
//!   with a `Message-ID` already filed is not filed again, even under another delegatee key.
//!   Polls of one recipient are serialized, so their record snapshots cannot race each other.
//! - **Key isolation.** An opened or unreadable verdict belongs to the delegatee public key
//!   that produced it. Recipients sharing records under different keys cannot hide mail from
//!   each other. Filed records stay shared because filing does not depend on the opening key.
//!   Separate recipients do not share a poll lock: concurrent polls can both file before
//!   either observes the other's filing record, preserving at-least-once delivery.
//! - **Retention.** An opened id is kept `H + σ` ([`OPENED_RETENTION_MS`]), after which no copy
//!   of its mail is served; a `Message-ID` is kept [`MESSAGE_ID_RETENTION_MS`].
//! - **One key.** Mail opens only under the node's current delegatee key. Mail sealed to a key
//!   the node no longer holds is lost, as mail not fetched within its lifetime is: a browser
//!   node takes a new key at every load, so mail a gateway accepted under the old one, within the
//!   descriptor's freshness window around a reload, never opens.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::lock::Mutex;
use rings_core::consts::MAX_TTL_MS;
use rings_core::consts::TS_OFFSET_TOLERANCE_MS;
use rings_core::dht::entry::Entry;
use rings_core::ecc::PublicKey;
use rings_core::lifecycle::StopToken;
use rings_core::storage::KvStorageInterface;
use rings_core::utils::get_epoch_ms;
use rings_runtime::MaybeSendSync;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

use super::envelope::MailDigest;
use super::mail_topic;
use super::open::open_mail_with_keys;
use super::open::OpenedMail;
use super::open::Opening;
use super::write_lowercase_hex;
use crate::processor::sleep_unless_stopped;
use crate::processor::Processor;

/// The default interval between two polls of the mail topic.
pub const DEFAULT_MAIL_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// The longest interval between two polls, `H / 2` (the schedule law).
pub const MAX_MAIL_POLL_INTERVAL: Duration = Duration::from_millis(MAX_TTL_MS / 2);
/// How long an opened mail id is kept: `H + σ`, after which no owner on a
/// clock within the skew tolerance serves an element of a mail written before.
pub const OPENED_RETENTION_MS: u128 = MAX_TTL_MS as u128 + TS_OFFSET_TOLERANCE_MS;
/// How long a filed `Message-ID` is kept: five days, the longest a sender retries a message
/// (RFC 5321 §4.5.4.1), so a resend never files a mail twice.
pub const MESSAGE_ID_RETENTION_MS: u128 = 5 * 24 * 3600 * 1000;

/// Where the recipient files the mail it opens: the mailbox store natively, IndexedDB in a
/// browser ([`KvInbox`]).
#[cfg_attr(rings_browser, async_trait(?Send))]
#[cfg_attr(rings_native, async_trait)]
pub trait MailInbox: MaybeSendSync {
    /// Why a mail could not be filed.
    type Error: std::error::Error + MaybeSendSync;

    /// File `mail`; `Ok` once it is kept.
    async fn file(&self, mail: &OpenedMail) -> Result<(), Self::Error>;
}

/// A mail as a [`KvInbox`] keeps it.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct StoredMail {
    /// The message.
    pub message: Vec<u8>,
    /// When it was filed, in milliseconds since Unix epoch.
    pub filed_at_ms: u128,
}

/// An inbox over a key-value store: each mail under `mail-<mail id>`.
pub struct KvInbox<K> {
    store: K,
}

impl<K> KvInbox<K> {
    /// The inbox keeping its mail in `store`.
    pub fn new(store: K) -> Self {
        Self { store }
    }

    /// The store the mail is kept in.
    pub fn store(&self) -> &K {
        &self.store
    }
}

#[cfg_attr(rings_browser, async_trait(?Send))]
#[cfg_attr(rings_native, async_trait)]
impl<K> MailInbox for KvInbox<K>
where K: KvStorageInterface<StoredMail> + MaybeSendSync
{
    type Error = rings_core::error::Error;

    async fn file(&self, mail: &OpenedMail) -> Result<(), Self::Error> {
        let stored = StoredMail {
            message: mail.message.clone(),
            filed_at_ms: get_epoch_ms(),
        };
        self.store
            .put(&format!("mail-{}", mail.mail_id), &stored)
            .await
    }
}

/// A record the recipient keeps.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum RecipientRecord {
    /// A mail id opened, or found unreadable, at `at_ms`: it is not opened again under the
    /// delegatee public key in its record name (the key-isolation law).
    Opened {
        /// When.
        at_ms: u128,
    },
    /// A mail with this deduplication key filed at `at_ms`.
    Filed {
        /// When.
        at_ms: u128,
    },
}

impl RecipientRecord {
    /// Whether this record has outlived its retention at `now_ms` (the retention law).
    pub fn outlived_at(&self, now_ms: u128) -> bool {
        let (since, retention) = match self {
            Self::Opened { at_ms } => (*at_ms, OPENED_RETENTION_MS),
            Self::Filed { at_ms } => (*at_ms, MESSAGE_ID_RETENTION_MS),
        };
        now_ms >= since.saturating_add(retention)
    }
}

/// Format the key-isolated [`RecipientRecord::Opened`] storage key
/// `opened-<public key hex>-<mail id hex>`.
///
/// Law: opening names bind both the delegatee public key and the mail digest, so distinct
/// delegatee public keys never share an opening verdict.
fn opened_record_name(delegatee_public_key: PublicKey<33>, mail_id: MailDigest) -> String {
    let mut name = String::with_capacity(7 + 66 + 1 + 64);
    name.push_str("opened-");
    let _ = write_lowercase_hex(&mut name, &delegatee_public_key.0);
    name.push('-');
    let _ = std::fmt::Write::write_fmt(&mut name, format_args!("{mail_id}"));
    name
}

/// Format the shared [`RecipientRecord::Filed`] storage key `filed-<deduplication key hex>`.
fn filed_record_name(key: MailDigest) -> String {
    format!("filed-{key}")
}

/// The key a mail is deduplicated by: `H(Message-ID)`, or its own id without one.
fn dedup_key(mail: &OpenedMail) -> MailDigest {
    mail.message_id().map_or(mail.mail_id, |message_id| {
        MailDigest::of(message_id.as_bytes())
    })
}

/// What one poll did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PollReport {
    /// Mail filed in the inbox.
    pub filed: usize,
    /// Mail opened whose `Message-ID` was filed before.
    pub duplicates: usize,
    /// Mail the node's key does not open.
    pub unreadable: usize,
    /// Mail a manifest lists a missing chunk of.
    pub incomplete: usize,
}

/// Why the recipient could not start or poll.
#[derive(Debug, Error)]
pub enum RecipientError<E> {
    /// The poll interval exceeds [`MAX_MAIL_POLL_INTERVAL`].
    #[error(
        "mail poll interval {interval:?} exceeds {MAX_MAIL_POLL_INTERVAL:?}, half the horizon"
    )]
    PollInterval {
        /// The interval asked for.
        interval: Duration,
    },
    /// The DHT key of the mail topic cannot be derived.
    #[error("no DHT key for the mail topic: {0}")]
    Key(#[source] rings_core::error::Error),
    /// The fetch of the mail topic failed.
    #[error("cannot fetch the mail topic: {0}")]
    Fetch(#[source] crate::error::Error),
    /// The records could not be read or written.
    #[error("mail records: {0}")]
    Records(#[source] rings_core::error::Error),
    /// The inbox did not file a mail.
    #[error("cannot file a mail: {0}")]
    Inbox(#[source] E),
}

/// Pure decision ADT classifying one `(Opening, Option<PublicKey<33>>)` against the current
/// `records` snapshot before any inbox or storage side effect runs.
enum OpeningAction {
    /// Manifest lists a missing chunk; retry on the next poll without recording `opened`.
    Incomplete,
    /// Already processed under a retained previous key; record `opened` under the active key (and
    /// the retained key when `opened_by` is present) without incrementing `PollReport`.
    AlreadyHandledByRetained {
        retained_opened_record: Option<String>,
        active_opened_record: String,
    },
    /// Unreadable by all held keys and not previously handled; increment `unreadable` and record
    /// `opened` under the active key.
    Unreadable { active_opened_record: String },
    /// Decrypted, but `Message-ID` was already filed; increment `duplicates` and record `opened`.
    Duplicate {
        retained_opened_record: Option<String>,
        active_opened_record: String,
    },
    /// Decrypted and fresh; file into `inbox`, record `filed`, and record `opened`.
    FileNew {
        mail: OpenedMail,
        filed_record: String,
        retained_opened_record: Option<String>,
        active_opened_record: String,
    },
}

/// Pure decision function mapping an `(Opening, Option<PublicKey<33>>)` and the current
/// `records` snapshot into an [`OpeningAction`].
///
/// Enforces the **Key-isolation law** and **Cross-key retention law** (#1153): if a retained
/// previous key already recorded `opened-<retained_pk>-<mail_id>`, re-observing the same mail
/// after key rotation records `opened-<active_pk>-<mail_id>` without double-counting `unreadable`
/// or `duplicates` in [`PollReport`].
fn classify_opening(
    opening: Opening,
    opened_by: Option<PublicKey<33>>,
    delegatee_public_key: PublicKey<33>,
    decryption_keys: &[rings_core::delegation::DelegateeKey],
    records: &BTreeMap<String, RecipientRecord>,
) -> OpeningAction {
    let mail_id = opening.mail_id();
    let active_opened_record = opened_record_name(delegatee_public_key, mail_id);
    let retained_opened_record = opened_by
        .filter(|&pk| pk != delegatee_public_key)
        .map(|pk| opened_record_name(pk, mail_id));
    let already_handled_by_retained_key = match &retained_opened_record {
        Some(record_name) => records.contains_key(record_name),
        None if opened_by.is_none() => decryption_keys.iter().skip(1).any(|key| {
            records.contains_key(&opened_record_name(key.delegatee_public_key(), mail_id))
        }),
        None => false,
    };

    match opening {
        Opening::Incomplete { .. } => OpeningAction::Incomplete,
        Opening::Unreadable { .. } if already_handled_by_retained_key => {
            OpeningAction::AlreadyHandledByRetained {
                retained_opened_record,
                active_opened_record,
            }
        }
        Opening::Unreadable { .. } => OpeningAction::Unreadable {
            active_opened_record,
        },
        Opening::Opened(_) if already_handled_by_retained_key => {
            OpeningAction::AlreadyHandledByRetained {
                retained_opened_record,
                active_opened_record,
            }
        }
        Opening::Opened(mail) => {
            let filed_record = filed_record_name(dedup_key(&mail));
            if records.contains_key(&filed_record) {
                OpeningAction::Duplicate {
                    retained_opened_record,
                    active_opened_record,
                }
            } else {
                OpeningAction::FileNew {
                    mail,
                    filed_record,
                    retained_opened_record,
                    active_opened_record,
                }
            }
        }
    }
}

/// A recipient: a node reading its own mail topic into `inbox`, keeping its records in
/// `records`.
pub struct Recipient<I, K> {
    processor: Arc<Processor>,
    inbox: I,
    records: K,
    interval: Duration,
    /// Serializes this recipient's polls so each observes the previous poll's filing records.
    poll_lock: Mutex<()>,
}

impl<I, K> Recipient<I, K>
where
    I: MailInbox,
    K: KvStorageInterface<RecipientRecord> + MaybeSendSync,
{
    /// The recipient of `processor`'s DID, filing into `inbox`, remembering in `records`, and
    /// polling every `interval`, which the schedule law bounds.
    pub fn start(
        processor: Arc<Processor>,
        inbox: I,
        records: K,
        interval: Duration,
    ) -> Result<Self, RecipientError<I::Error>> {
        if interval > MAX_MAIL_POLL_INTERVAL {
            return Err(RecipientError::PollInterval { interval });
        }
        Ok(Self {
            processor,
            inbox,
            records,
            interval,
            poll_lock: Mutex::new(()),
        })
    }

    /// The inbox mail is filed in.
    pub fn inbox(&self) -> &I {
        &self.inbox
    }

    /// Read the mail topic once, file what opens, and remember it (the poll of this module).
    pub async fn poll(&self) -> Result<PollReport, RecipientError<I::Error>> {
        let _poll_guard = self.poll_lock.lock().await;
        let did = self.processor.did();
        let key = Entry::gen_did(&mail_topic(did)).map_err(RecipientError::Key)?;
        let entry = self
            .processor
            .fetch_storage_entry(key)
            .await
            .map_err(RecipientError::Fetch)?;
        let now_ms = get_epoch_ms();
        let mut records = self.retained_records(now_ms).await?;
        let Some(entry) = entry else {
            return Ok(PollReport::default());
        };
        let delegatee_key = self.processor.delegatee_key();
        let delegatee_public_key = delegatee_key.delegatee_public_key();
        let decryption_keys = self.processor.decryption_keys_at(now_ms);
        let fresh = |mail_id: &MailDigest| {
            !records.contains_key(&opened_record_name(delegatee_public_key, *mail_id))
        };
        let openings = open_mail_with_keys(&entry.data, did, &decryption_keys, fresh);
        let mut report = PollReport::default();
        for (opening, opened_by) in openings {
            match classify_opening(
                opening,
                opened_by,
                delegatee_public_key,
                &decryption_keys,
                &records,
            ) {
                OpeningAction::Incomplete => {
                    report.incomplete = report.incomplete.saturating_add(1);
                }
                OpeningAction::AlreadyHandledByRetained {
                    retained_opened_record,
                    active_opened_record,
                } => {
                    self.remember_opened_pair(
                        &mut records,
                        retained_opened_record,
                        active_opened_record,
                        now_ms,
                    )
                    .await?;
                }
                OpeningAction::Unreadable {
                    active_opened_record,
                } => {
                    report.unreadable = report.unreadable.saturating_add(1);
                    self.remember_opened_pair(&mut records, None, active_opened_record, now_ms)
                        .await?;
                }
                OpeningAction::Duplicate {
                    retained_opened_record,
                    active_opened_record,
                } => {
                    report.duplicates = report.duplicates.saturating_add(1);
                    self.remember_opened_pair(
                        &mut records,
                        retained_opened_record,
                        active_opened_record,
                        now_ms,
                    )
                    .await?;
                }
                OpeningAction::FileNew {
                    mail,
                    filed_record,
                    retained_opened_record,
                    active_opened_record,
                } => {
                    self.inbox
                        .file(&mail)
                        .await
                        .map_err(RecipientError::Inbox)?;
                    self.remember(&mut records, filed_record, RecipientRecord::Filed {
                        at_ms: now_ms,
                    })
                    .await?;
                    report.filed = report.filed.saturating_add(1);
                    self.remember_opened_pair(
                        &mut records,
                        retained_opened_record,
                        active_opened_record,
                        now_ms,
                    )
                    .await?;
                }
            }
        }
        Ok(report)
    }

    /// Record [`RecipientRecord::Opened`] for the retained key (if any) and the active key.
    async fn remember_opened_pair(
        &self,
        records: &mut BTreeMap<String, RecipientRecord>,
        retained_opened_record: Option<String>,
        active_opened_record: String,
        now_ms: u128,
    ) -> Result<(), RecipientError<I::Error>> {
        if let Some(retained_name) = retained_opened_record {
            self.remember(records, retained_name, RecipientRecord::Opened {
                at_ms: now_ms,
            })
            .await?;
        }
        self.remember(records, active_opened_record, RecipientRecord::Opened {
            at_ms: now_ms,
        })
        .await
    }

    /// Poll now and then every interval until `stop`, logging a failed poll and polling again.
    pub async fn run(&self, stop: StopToken) {
        loop {
            match self.poll().await {
                Ok(report) if report.filed > 0 => {
                    tracing::info!(filed = report.filed, "mail: filed new mail");
                }
                Ok(_) => {}
                Err(error) => tracing::warn!("mail: poll failed: {error}"),
            }
            match sleep_unless_stopped(self.interval, &stop, &StopToken::never()).await {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    tracing::warn!("mail: stopping after a timer error: {error:?}");
                    return;
                }
            }
        }
    }

    /// The records retained at `now_ms`, after removing those that outlived their retention.
    async fn retained_records(
        &self,
        now_ms: u128,
    ) -> Result<BTreeMap<String, RecipientRecord>, RecipientError<I::Error>> {
        let (expired, retained): (Vec<_>, Vec<_>) = self
            .records
            .get_all()
            .await
            .map_err(RecipientError::Records)?
            .into_iter()
            .partition(|(_, record)| record.outlived_at(now_ms));
        for (name, _) in expired {
            self.records
                .remove(&name)
                .await
                .map_err(RecipientError::Records)?;
        }
        Ok(retained.into_iter().collect())
    }

    /// Keep `record` under `name`, in the store and in `records`, this poll's view of it: two
    /// copies of one mail opened by one poll are filed once.
    async fn remember(
        &self,
        records: &mut BTreeMap<String, RecipientRecord>,
        name: String,
        record: RecipientRecord,
    ) -> Result<(), RecipientError<I::Error>> {
        self.records
            .put(&name, &record)
            .await
            .map_err(RecipientError::Records)?;
        records.insert(name, record);
        Ok(())
    }
}

/// Native witnesses of key-isolated opening records and shared filing records.
#[cfg(all(test, rings_native))]
mod tests;
