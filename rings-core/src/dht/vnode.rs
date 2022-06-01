#![warn(missing_docs)]
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::err::{Error, Result};
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::MessageRelay;
use num_bigint::BigUint;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use std::str::FromStr;

/// VNode Types
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum VNodeType {
    /// Data: Encoded data stored in DHT
    Data,
    /// SubRing: Finger table of a SubRing
    SubRing,
    /// RelayMessage: A Relayed but unreach message, which is stored on it's successor
    RelayMessage,
}

/// A Virtual Node is a Node that dont have real network address.
/// The Address of a Virtual Node is virutal,
/// For Encoded Data, it's sha1 of data, for a SubRing, it's sha1 of SubRing's name,
/// and for the RelayedMessage, it's the target address of message plus 1 (for ensure that the message is
/// sent to the successor of target), thus while target Node going online, it will sync from it's successor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VirtualNode {
    /// address of vnode
    pub address: Did,
    /// all data of vnode should be encoded
    pub data: Vec<Encoded>,
    /// vnode type
    pub kind: VNodeType,
}

impl VirtualNode {
    /// VirtualNode should have it's virtual address.
    pub fn did(&self) -> Did {
        self.address
    }
}

impl<T> TryFrom<MessageRelay<T>> for VirtualNode
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
            kind: VNodeType::RelayMessage,
        })
    }
}

impl TryFrom<Encoded> for VirtualNode {
    type Error = Error;
    fn try_from(e: Encoded) -> Result<Self> {
        let address: HashStr = e.value().into();
        Ok(Self {
            address: Did::from_str(&address.inner())?,
            data: vec![e],
            kind: VNodeType::Data,
        })
    }
}

impl VirtualNode {
    /// concat data of a virtual Node
    /// We do not needs to check the type of VNode because two VNode with same address but
    /// has different Type is incapable
    pub fn concat(a: &Self, b: &Self) -> Result<Self> {
        if a.address != b.address {
            Err(Error::AddressNotEqual)
        } else {
            Ok(Self {
                address: a.address,
                data: [&a.data[..], &b.data[..]].concat(),
                kind: a.kind.clone(),
            })
        }
    }
}
