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
pub(crate) use payload::LinkControl;
pub(crate) use payload::LinkFrame;
pub use payload::MessagePayload;
pub use payload::PayloadSender;
pub(crate) use payload::PerSlot;
pub(crate) use payload::SessionRef;
pub(crate) use payload::SlotEncoding;
pub use payload::Transaction;
pub(crate) use payload::WirePayload;

mod quota;
pub use quota::OriginQuota;
pub use quota::OriginQuotaArithmeticError;
pub use quota::OriginQuotaConfig;
pub use quota::OriginQuotaConfigError;
pub use quota::OriginQuotaCounters;
pub use quota::OriginQuotaError;
pub use quota::OriginQuotaInstant;
pub use quota::OriginQuotaKey;
pub use quota::OriginQuotaLaneConfig;
pub use quota::OriginQuotaLaneCounters;
pub use quota::OriginQuotaVerdict;
pub use quota::DEFAULT_ORIGIN_QUOTA_BYTES_PER_SECOND;
pub use quota::DEFAULT_ORIGIN_QUOTA_BYTE_BURST;
pub use quota::DEFAULT_ORIGIN_QUOTA_MESSAGES_PER_SECOND;
pub use quota::DEFAULT_ORIGIN_QUOTA_MESSAGE_BURST;
pub use quota::DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE;
pub use types::MessageCategory;

mod replay;
pub use replay::observe;
pub use replay::ReplayCounters;
pub use replay::ReplaySnapshot;
pub use replay::ReplayStorage;
pub use replay::SequenceState;
pub use replay::SequenceVerdict;
pub use replay::StreamKey;
pub use replay::TransactionDigest;
pub use replay::TransactionForkEvidence;
pub(crate) use replay::TransactionReplay;
pub use replay::TRANSACTION_REPLAY_STREAM_CAPACITY;
pub use replay::TRANSACTION_REPLAY_WINDOW;

mod service_receipt;
#[cfg(test)]
pub(crate) use service_receipt::test_probe_request;
pub use service_receipt::ProbeAcknowledgement;
pub use service_receipt::ProbeCompletion;
pub use service_receipt::ProbeOffer;
pub use service_receipt::ProbeRequest;
pub use service_receipt::ProvisionalEpoch;
pub use service_receipt::ProvisionalServiceClaim;
pub use service_receipt::ProvisionalServiceReceipt;
pub use service_receipt::ServiceKind;
pub use service_receipt::ServiceReceiptDigest;
pub use service_receipt::ServiceReceiptError;
pub use service_receipt::PROVISIONAL_RECEIPT_EPOCH_SECS;

pub mod types;
pub use types::*;

pub mod handlers;
pub use handlers::storage::ChordStorageInterface;
pub use handlers::storage::ChordStorageInterfaceCacheChecker;
pub use handlers::HandleMsg;
pub use handlers::MessageHandler;

mod protocols;
pub use protocols::DomainTag;
pub use protocols::HopBudget;
pub use protocols::MessageRelay;
pub use protocols::MessageSigner;
pub use protocols::MessageVerification;
pub use protocols::MessageVerificationExt;
pub use protocols::SigningDomain;
