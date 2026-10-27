//! Mail through gateways (#1117): the mailbox as the recipient of its node's mail topic, filing
//! every mail it opens in the INBOX.
//!
//! ```text
//!   <dir>/recipient/   the recipient's records: opened mail, filed Message-IDs, retired keys
//!   mail topic ─poll─▶ open ─▶ INBOX
//! ```
//!
//! The gateway already traced the message (`Return-Path`, `Received`), so it is filed as it
//! opens.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rings_node::mail::open::OpenedMail;
use rings_node::mail::recipient::MailInbox;
use rings_node::mail::recipient::Recipient;
use rings_node::mail::recipient::RecipientError;
use rings_node::mail::recipient::DEFAULT_MAIL_POLL_INTERVAL;
use rings_node::prelude::rings_core::storage::file::FileStorage;
use rings_node::prelude::rings_core::storage::RecordAuthority;
use rings_node::processor::Processor;

use crate::store::Store;
use crate::store::StoreError;
use crate::store::INBOX;

/// Bytes the recipient's records may take on disk: far above what its retention leaves.
const RECORDS_CAPACITY: u32 = 64 * 1024 * 1024;

/// The mailbox's INBOX as a recipient's inbox.
pub struct StoreInbox(pub Arc<Store>);

#[async_trait]
impl MailInbox for StoreInbox {
    type Error = StoreError;

    async fn file(&self, mail: &OpenedMail) -> Result<(), StoreError> {
        let now = chrono::Utc::now().timestamp();
        self.0
            .append(INBOX, &mail.message, Default::default(), now)
            .await
            .map(drop)
    }
}

/// The recipient filing `processor`'s mail in `store`, its records under `records`: kept
/// across restarts and never evicted, since they hold the retired keys mail still opens with.
pub async fn recipient(
    processor: Arc<Processor>,
    store: Arc<Store>,
    records: &Path,
) -> Result<Recipient<StoreInbox, FileStorage>, RecipientError<StoreError>> {
    let records = FileStorage::new_with_cap_path_and_authority(
        RECORDS_CAPACITY,
        records,
        RecordAuthority::Retained,
    )
    .await
    .map_err(RecipientError::Records)?;
    Recipient::start(
        processor,
        StoreInbox(store),
        records,
        DEFAULT_MAIL_POLL_INTERVAL,
    )
    .await
}
