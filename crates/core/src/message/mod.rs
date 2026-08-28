//! Message and MessageHandler
mod encoder;
pub use encoder::Decoder;
pub use encoder::Encoded;
pub use encoder::Encoder;

pub mod e2e;

mod effects;
#[cfg(all(test, feature = "wasm", target_family = "wasm"))]
pub(crate) use effects::browser_task_yield_guard_counts_for_test;
#[cfg(all(test, feature = "wasm", target_family = "wasm"))]
pub(crate) use effects::reset_browser_task_yield_guard_counts_for_test;
#[cfg(all(test, feature = "wasm", target_family = "wasm"))]
pub(crate) use effects::yield_browser_task;
pub(crate) use effects::yield_core_actor_step;
#[cfg(all(test, feature = "wasm", target_family = "wasm"))]
pub(crate) use effects::CORE_ACTOR_BROWSER_YIELD_INTERVAL;

mod payload;
pub use payload::decode_gzip_data;
pub use payload::encode_data_gzip;
pub use payload::from_gzipped_data;
pub use payload::gzip_data;
pub use payload::MessagePayload;
pub use payload::PayloadSender;
pub use payload::Transaction;

pub mod types;
pub use types::*;

pub mod handlers;
pub use handlers::storage::ChordStorageInterface;
pub use handlers::storage::ChordStorageInterfaceCacheChecker;
pub use handlers::HandleMsg;
pub use handlers::MessageHandler;

mod protocols;
pub use protocols::MessageRelay;
pub use protocols::MessageVerification;
pub use protocols::MessageVerificationExt;
pub use protocols::ReportReturnPolicy;
