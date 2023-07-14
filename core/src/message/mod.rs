//! Message and MessageHandler
mod encoder;
pub use encoder::Decoder;
pub use encoder::Encoded;
pub use encoder::Encoder;

mod payload;
pub use payload::decode_gzip_data;
pub use payload::encode_data_gzip;
pub use payload::from_gzipped_data;
pub use payload::gzip_data;
pub use payload::MessagePayload;
pub use payload::OriginVerificationGen;
pub use payload::PayloadSender;

pub mod types;
pub use types::*;

pub mod handlers;
pub use handlers::storage::ChordStorageInterface;
pub use handlers::storage::ChordStorageInterfaceCacheChecker;
pub use handlers::subring::SubringInterface;
pub use handlers::CallbackFn;
pub use handlers::HandleMsg;
pub use handlers::MessageCallback;
pub use handlers::MessageHandler;
pub use handlers::MessageHandlerEvent;
pub use handlers::ValidatorFn;

mod protocols;
pub use protocols::MessageRelay;
