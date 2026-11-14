/// Redundant setting of entry data storage
pub const DATA_REDUNDANT: u16 = 6;
/// Connect Behaviour
pub const CONNECT_FAILED_LIMIT: u64 = 3;
/// Message Send Behaviour
pub const MSG_SEND_FAILED_LIMIT: u64 = 10;
/// Message Received Behaviour
pub const MSG_RECV_FAILED_LIMIT: u64 = 10;

/// Default storage capacity in bytes for node storage backends (DHT entries, measurements, and
/// provisional evidence).
pub const DEFAULT_STORAGE_CAPACITY: u32 = 200_000_000;
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
/// Byte budget of each retained browser mail store (`mail-inbox` and `mail-records`).
pub const MAIL_STORAGE_CAPACITY: u32 = 64 * 1024 * 1024;
/// Byte budget of the store of the data namespace's remove sides (#920, #907): at least the
/// `REMOVAL_STORE_MAX_REMOVES` items the accounting admits, each filed in at most
/// `REMOVAL_STORED_ITEM_WIRE_MAX` bytes when every record holds one (about 19.6 MB). The store
/// is retained and evicts nothing: a remove beyond the budget is refused, never another one
/// evicted.
pub const DHT_REMOVAL_STORAGE_CAPACITY: u32 = 24 * 1024 * 1024;
const _: () = assert!(
    rings_core::consts::REMOVAL_STORE_MAX_REMOVES
        * rings_core::consts::REMOVAL_STORED_ITEM_WIRE_MAX
        <= DHT_REMOVAL_STORAGE_CAPACITY as usize
);
