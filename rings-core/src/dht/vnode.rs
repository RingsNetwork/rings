#![warn(missing_docs)]
use std::collections::BTreeSet;
use std::str::FromStr;

use num_bigint::BigUint;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::subring::SubRing;
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::err::Error;
use crate::err::Result;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::MessagePayload;

/// VNode Types
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

impl<T> TryFrom<MessagePayload<T>> for VirtualNode
where T: Serialize + DeserializeOwned
{
    type Error = Error;
    fn try_from(msg: MessagePayload<T>) -> Result<Self> {
        let address = BigUint::from(msg.addr) + BigUint::from(1u16);
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

impl TryFrom<String> for VirtualNode {
    type Error = Error;
    fn try_from(s: String) -> Result<Self> {
        let encoded_message = s.encode()?;
        encoded_message.try_into()
    }
}

impl VirtualNode {
    /// concat data of a virtual Node
    /// We do not needs to check the type of VNode because two VNode with same address but
    /// has different Type is incapable
    pub fn concat(a: &Self, b: &Self) -> Result<Self> {
        match &a.kind {
            VNodeType::RelayMessage => {
                if a.address != b.address {
                    Err(Error::DidNotEqual)
                } else {
                    Ok(Self {
                        address: a.address,
                        data: [&a.data[..], &b.data[..]].concat(),
                        kind: a.kind.clone(),
                    })
                }
            }
            VNodeType::Data => Ok(a.clone()),
            VNodeType::SubRing => {
                // if subring exists, just join creator to new subring
                let decoded_a: String = a.data[0].decode()?;
                let decoded_b: String = a.data[0].decode()?;
                let mut subring_a: SubRing =
                    serde_json::from_str(&decoded_a).map_err(Error::Deserialize)?;
                let subring_b: SubRing =
                    serde_json::from_str(&decoded_b).map_err(Error::Deserialize)?;
                subring_a.finger.join(subring_b.creator, BTreeSet::new());
                subring_a.try_into()
            }
        }
    }
}
