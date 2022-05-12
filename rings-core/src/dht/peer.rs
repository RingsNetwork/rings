use crate::dht::Did;
use crate::err::{Error, Result};
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::MessageRelay;
use crate::ecc::HashStr;
use num_bigint::BigUint;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use std::str::FromStr;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VirtualPeer {
    pub address: Did,
    pub data: Vec<Encoded>,
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
        let address = BigUint::from(Did::from(msg.addr)) + BigUint::from(1u16);
        let data = msg.encode()?;
        Ok(Self {
            address: address.into(),
            data: vec![data],
        })
    }
}

impl TryFrom<Encoded> for VirtualPeer {
    type Error = Error;
    fn try_from(e: Encoded) -> Result<Self> {
        let address: HashStr = e.value().into();
        Ok(Self {
            address: Did::from_str(&address.inner())?,
            data: vec!(e)
        })
    }
}

impl VirtualPeer {
    pub fn concat(a: &Self, b: &Self) -> Result<Self> {
        if a.address != b.address {
            Err(Error::AddressNotEqual)
        } else {
            Ok(Self {
                address: a.address,
                data: [&a.data[..], &b.data[..]].concat(),
            })
        }
    }
}
