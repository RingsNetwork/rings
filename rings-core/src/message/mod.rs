//! Message and MessageHandler

mod encoder;
pub use encoder::Decoder;
pub use encoder::Encoded;
pub use encoder::Encoder;

mod payload;
pub use payload::MessagePayload;
pub use payload::OriginVerificationGen;
pub use payload::PayloadSender;

mod types;
pub use types::*;

pub mod handlers;
pub use handlers::HandleMsg;
pub use handlers::MessageCallback;
pub use handlers::MessageHandler;

mod protocols;
pub use protocols::MessageRelay;
pub use protocols::RelayMethod;
