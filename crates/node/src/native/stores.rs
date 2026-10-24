//! The native stores of a node, each in its own directory and opened at its authority
//! (durable-records, Table I): the one layout the daemon and the FFI provider share.
//!
//! ```text
//! <data>                  DHT data store            Disposable      configured budget
//! <data>/../relay-inbox        relay carriers       Retained        RELAY_INBOX_STORAGE_CAPACITY
//! <data>/../transaction-replay replay windows       Authoritative   TRANSACTION_REPLAY_STORAGE_CAPACITY
//! <data>/../onion-entry-guards entry guards         DurableCache    ONION_ENTRY_GUARD_STORAGE_CAPACITY
//! <measure>               peer measurements         Disposable      configured budget
//! <measure>/../provisional-evidence                 Disposable      the measure budget
//! ```
//!
//! Every store beside the data or the measure store is a sibling directory of it
//! ([`sibling_storage_path`]). Each store locks its own directory at open (#1039), so a second
//! daemon or FFI provider given a store an open node holds fails to open it, with
//! `StorageDirectoryLocked`, instead of writing beside it.

use std::path::Path;

use rings_core::dht::EntryStorage;
use rings_core::message::ReplayStorage;
use rings_core::storage::file::FileStorage;
use rings_core::storage::RecordAuthority;

use crate::error::Error;
use crate::error::Result;
use crate::measure::EvidenceStorage;
use crate::measure::MeasureStorage;
use crate::native::config::StorageConfig;
use crate::native::config::DEFAULT_STORAGE_CAPACITY;
use crate::onion::OnionEntryGuardStorage;

/// Byte budget of the onion entry-guard store.
pub const ONION_ENTRY_GUARD_STORAGE_CAPACITY: u32 = 64 * 1024;
/// Byte budget of the replay store: at least every record of a full store
/// (`rings_core::message::TRANSACTION_REPLAY_STORE_MAX_BYTES`, about 27 MiB). The store is
/// authoritative and evicts nothing: a write beyond the budget fails, which fails that
/// transition closed, so the budget must never be reached.
pub const TRANSACTION_REPLAY_STORAGE_CAPACITY: u32 = 40 * 1024 * 1024;
/// Byte budget of the relay-inbox store: the carriers this node holds or owns, each at most
/// `RELAY_INBOX_MAX_LEN` held messages. The store is retained and evicts nothing (#1021): a
/// hold beyond the budget is refused, so the sender's message is not held, instead of evicting
/// mail already held.
pub const RELAY_INBOX_STORAGE_CAPACITY: u32 = 64 * 1024 * 1024;
/// Directory of the onion entry-guard store, beside the data store.
pub const ONION_ENTRY_GUARD_STORAGE_DIR: &str = "onion-entry-guards";
/// Directory of the transaction replay store, beside the data store.
pub const TRANSACTION_REPLAY_STORAGE_DIR: &str = "transaction-replay";
/// Directory of the relay-inbox store, beside the data store.
pub const RELAY_INBOX_STORAGE_DIR: &str = "relay-inbox";
/// Directory of the provisional-evidence store, beside the measure store.
pub const PROVISIONAL_EVIDENCE_STORAGE_DIR: &str = "provisional-evidence";
/// Directory of the DHT data store under a storage root ([`NodeStores::open_under`]).
pub const DATA_STORAGE_DIR: &str = "data";
/// Directory of the measure store under a storage root ([`NodeStores::open_under`]).
pub const MEASURE_STORAGE_DIR: &str = "measure";

/// The path of the store directory `dir` beside the store at `storage_path`: in its parent, or
/// in the working directory when it has none (pure).
pub fn sibling_storage_path(storage_path: &str, dir: &str) -> String {
    Path::new(storage_path)
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(dir)
        .to_string_lossy()
        .to_string()
}

/// Every native store of a node, opened at its authority (module docs).
pub struct NodeStores {
    /// The DHT data store (disposable).
    pub data: EntryStorage,
    /// The relay-inbox store (retained).
    pub relay: EntryStorage,
    /// The peer measurement store (disposable).
    pub measure: MeasureStorage,
    /// The provisional-evidence store (disposable).
    pub evidence: EvidenceStorage,
    /// The onion entry-guard store (durable cache).
    pub guards: OnionEntryGuardStorage,
    /// The transaction replay store (authoritative).
    pub replay: ReplayStorage,
}

/// Open the file store at `path` under `capacity` bytes with `authority`.
async fn open_file(path: String, capacity: u32, authority: RecordAuthority) -> Result<FileStorage> {
    FileStorage::new_with_cap_path_and_authority(capacity, path, authority)
        .await
        .map_err(Error::Storage)
}

impl NodeStores {
    /// Open every store of a node whose data store is `data` and measure store is `measure`,
    /// the others beside them (module docs).
    pub async fn open(data: &StorageConfig, measure: &StorageConfig) -> Result<Self> {
        let beside_data = |dir| sibling_storage_path(&data.path, dir);
        Ok(Self {
            data: Box::new(
                open_file(
                    data.path.clone(),
                    data.capacity,
                    RecordAuthority::Disposable,
                )
                .await?,
            ),
            relay: Box::new(
                open_file(
                    beside_data(RELAY_INBOX_STORAGE_DIR),
                    RELAY_INBOX_STORAGE_CAPACITY,
                    RecordAuthority::Retained,
                )
                .await?,
            ),
            measure: Box::new(
                open_file(
                    measure.path.clone(),
                    measure.capacity,
                    RecordAuthority::Disposable,
                )
                .await?,
            ),
            evidence: Box::new(
                open_file(
                    sibling_storage_path(&measure.path, PROVISIONAL_EVIDENCE_STORAGE_DIR),
                    measure.capacity,
                    RecordAuthority::Disposable,
                )
                .await?,
            ),
            guards: Box::new(
                open_file(
                    beside_data(ONION_ENTRY_GUARD_STORAGE_DIR),
                    ONION_ENTRY_GUARD_STORAGE_CAPACITY,
                    RecordAuthority::DurableCache,
                )
                .await?,
            ),
            replay: Box::new(
                open_file(
                    beside_data(TRANSACTION_REPLAY_STORAGE_DIR),
                    TRANSACTION_REPLAY_STORAGE_CAPACITY,
                    RecordAuthority::Authoritative,
                )
                .await?,
            ),
        })
    }

    /// Open every store of a node under the storage root `root`: the data store at
    /// `<root>/data` and the measure store at `<root>/measure`, each under
    /// [`DEFAULT_STORAGE_CAPACITY`], and the others beside them (the FFI provider's layout).
    pub async fn open_under(root: &Path) -> Result<Self> {
        let store = |dir: &str| root.join(dir).to_string_lossy().to_string();
        Self::open(
            &StorageConfig::new(&store(DATA_STORAGE_DIR), DEFAULT_STORAGE_CAPACITY),
            &StorageConfig::new(&store(MEASURE_STORAGE_DIR), DEFAULT_STORAGE_CAPACITY),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every store beside the data or the measure store is a sibling directory of it, in the
    /// working directory when the store path has no parent.
    #[test]
    fn test_sibling_stores_sit_beside_their_store() {
        for dir in [
            ONION_ENTRY_GUARD_STORAGE_DIR,
            TRANSACTION_REPLAY_STORAGE_DIR,
            RELAY_INBOX_STORAGE_DIR,
            PROVISIONAL_EVIDENCE_STORAGE_DIR,
        ] {
            assert_eq!(
                sibling_storage_path(".rings/data", dir),
                format!(".rings/{dir}")
            );
            assert_eq!(
                sibling_storage_path("/tmp/rings/data", dir),
                format!("/tmp/rings/{dir}")
            );
            assert_eq!(sibling_storage_path("data", dir), format!("./{dir}"));
        }
    }

    /// A full replay store always fits the replay store's budget, so the stream-count bound,
    /// not the store's budget, is what fails closed.
    #[test]
    fn test_transaction_replay_storage_holds_a_full_store(
    ) -> std::result::Result<(), std::num::TryFromIntError> {
        let capacity = usize::try_from(TRANSACTION_REPLAY_STORAGE_CAPACITY)?;
        assert!(rings_core::message::TRANSACTION_REPLAY_STORE_MAX_BYTES <= capacity);
        Ok(())
    }

    /// The FFI layout opens every store, each in its own directory under the root.
    #[tokio::test]
    async fn test_open_under_lays_every_store_out_under_the_root() -> Result<()> {
        let root = std::env::temp_dir().join(format!("rings-node-stores-{}", uuid::Uuid::new_v4()));
        let stores = NodeStores::open_under(&root).await;
        let mut dirs = std::fs::read_dir(&root)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        dirs.sort();
        let _ = std::fs::remove_dir_all(&root);
        stores?;
        assert_eq!(dirs, [
            DATA_STORAGE_DIR,
            MEASURE_STORAGE_DIR,
            ONION_ENTRY_GUARD_STORAGE_DIR,
            PROVISIONAL_EVIDENCE_STORAGE_DIR,
            RELAY_INBOX_STORAGE_DIR,
            TRANSACTION_REPLAY_STORAGE_DIR,
        ]);
        Ok(())
    }

    /// A second node over the stores of an open node fails at open, at the first store it
    /// shares (#1039), whether it is a daemon or an FFI provider: both open through
    /// [`NodeStores`]. Once the first node drops, the root opens again.
    #[tokio::test]
    async fn test_a_second_node_on_open_stores_fails_at_open() -> Result<()> {
        let root = std::env::temp_dir().join(format!("rings-node-lock-{}", uuid::Uuid::new_v4()));
        let first = NodeStores::open_under(&root).await;
        let second = NodeStores::open_under(&root).await;
        let first_opened = first.is_ok();
        drop(first);
        let reopened = NodeStores::open_under(&root).await;
        let _ = std::fs::remove_dir_all(&root);
        assert!(first_opened);
        assert!(matches!(
            second,
            Err(Error::Storage(
                rings_core::error::Error::StorageDirectoryLocked(_)
            ))
        ));
        reopened.map(drop)
    }
}
