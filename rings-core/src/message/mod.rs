//! Message and MessageHandler
/// msg chunk and it's helper function
pub mod chunk;
mod encoder;
pub use encoder::Decoder;
pub use encoder::Encoded;
pub use encoder::Encoder;

mod payload;
pub use payload::decode_gzip_data;
pub use payload::from_gzipped_data;
pub use payload::gzip_data;
pub use payload::MessagePayload;
pub use payload::OriginVerificationGen;
pub use payload::PayloadSender;

mod types;
pub use types::*;

mod handlers;
pub use handlers::storage::TChordStorage;
pub use handlers::CallbackFn;
pub use handlers::HandleMsg;
pub use handlers::MessageCallback;
pub use handlers::MessageHandler;
pub use handlers::ValidatorFn;

mod protocols;
pub use protocols::MessageRelay;
pub use protocols::RelayMethod;
