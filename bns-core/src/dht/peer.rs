use crate::dht::Did;
use crate::err::{Error, Result};
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::MessageRelay;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VirtualPeer {
    address: Did,
    pub data: Encoded,
}

impl VirtualPeer {
    pub fn did(&self) -> Did {
        self.address
    }
}

impl<T> TryFrom<MessageRelay<T>> for VirtualPeer
where
    T: Serialize + DeserializeOwned,
{
    type Error = Error;
    fn try_from(msg: MessageRelay<T>) -> Result<Self> {
        Ok(Self {
            address: msg.addr.into(),
            data: msg.encode()?,
        })
    }
}
