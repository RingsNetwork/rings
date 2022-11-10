use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::vnode::VirtualNode;
use crate::dht::Did;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::err::Error;
use crate::err::Result;
use crate::peer::PeerService;

/// MessageType use to ask for connection, send to remote with transport_uuid and handshake_info.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct ConnectNodeSend {
    pub transport_uuid: String,
    pub handshake_info: String,
}

/// MessageType report to origin that already connected.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct AlreadyConnected;

/// MessageType report to origin with own transport_uuid and handshake_info.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct ConnectNodeReport {
    pub transport_uuid: String,
    pub handshake_info: String,
}

/// MessageType use to find successor in a chord ring.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct FindSuccessorSend {
    pub id: Did,
    pub strict: bool,
    pub then: FindSuccessorThen,
}

/// MessageType use to report origin node with report message.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FindSuccessorReport {
    pub id: Did,
    pub handler: FindSuccessorReportHandler,
}

/// MessageType use to notify predecessor, ask for update finger tables.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NotifyPredecessorSend {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType report to origin node.
pub struct NotifyPredecessorReport {
    pub id: Did,
}

/// MessageType use to join chord ring, add did into fingers table.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JoinDHT {
    pub id: Did,
    pub services: Vec<PeerService>,
}

/// MessageType use to leave chord ring.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LeaveDHT {
    pub id: Did,
}

/// MessageType use to search virtual node.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SearchVNode {
    pub id: Did,
}

/// MessageType report to origin found virtual node.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FoundVNode {
    pub data: Vec<VirtualNode>,
}

/// MessageType ask to store data/message into virtual node.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct StoreVNode {
    pub data: Vec<VirtualNode>,
}

/// MessageType contains multi messages.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct MultiCall {
    pub messages: Vec<Message>,
}

/// MessageType after `FindSuccessorSend` and syncing data.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SyncVNodeWithSuccessor {
    pub data: Vec<VirtualNode>,
}

/// MessageType use to join virtual node in subring.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JoinSubRing {
    pub did: Did,
}

/// MessageType use to customize message, will be handle by `custom_message` method.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CustomMessage(pub Vec<u8>);

/// A enum about Encrypted and Plain types.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub enum MaybeEncrypted<T> {
    Encrypted(Vec<u8>),
    Plain(T),
}

/// MessageType enum Report contain FindSuccessorSend.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FindSuccessorThen {
    Report(FindSuccessorReportHandler),
}

/// MessageType enum handle when meet the last node.
/// - None: do nothing but return.
/// - Connect: connect origin node.
/// - FixFingerTable: update fingers table.
/// - SyncStorage: syncing data in virtual node.
/// - CustomCallback: custom callback handle by `custom_message` method.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FindSuccessorReportHandler {
    None,
    Connect,
    FixFingerTable,
    SyncStorage,
    CustomCallback(u8),
}

/// A collection MessageType use for unified management.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum Message {
    MultiCall(MultiCall),
    JoinDHT(JoinDHT),
    LeaveDHT(LeaveDHT),
    ConnectNodeSend(ConnectNodeSend),
    AlreadyConnected(AlreadyConnected),
    ConnectNodeReport(ConnectNodeReport),
    FindSuccessorSend(FindSuccessorSend),
    FindSuccessorReport(FindSuccessorReport),
    NotifyPredecessorSend(NotifyPredecessorSend),
    NotifyPredecessorReport(NotifyPredecessorReport),
    SearchVNode(SearchVNode),
    FoundVNode(FoundVNode),
    StoreVNode(StoreVNode),
    SyncVNodeWithSuccessor(SyncVNodeWithSuccessor),
    JoinSubRing(JoinSubRing),
    CustomMessage(MaybeEncrypted<CustomMessage>),
}

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Message {
    pub fn custom(msg: &[u8], pubkey: Option<PublicKey>) -> Result<Message> {
        let data = CustomMessage(msg.to_vec());
        let msg = MaybeEncrypted::new(data, pubkey)?;
        Ok(Message::CustomMessage(msg))
    }
}

impl<T> MaybeEncrypted<T>
where T: Serialize + DeserializeOwned
{
    pub fn new(data: T, pubkey: Option<PublicKey>) -> Result<Self> {
        if let Some(pubkey) = pubkey {
            let msg = bincode::serialize(&data).map_err(Error::BincodeSerialize)?;
            let pubkey: libsecp256k1::PublicKey = pubkey.try_into()?;
            let cipher = ecies::encrypt(&pubkey.serialize(), &msg)
                .map_err(Error::MessageEncryptionFailed)?;
            Ok(MaybeEncrypted::Encrypted(cipher))
        } else {
            Ok(MaybeEncrypted::Plain(data))
        }
    }

    pub fn decrypt(self, key: SecretKey) -> Result<(T, bool)> {
        match self {
            MaybeEncrypted::Plain(msg) => Ok((msg, false)),
            MaybeEncrypted::Encrypted(cipher) => {
                let plain = ecies::decrypt(&key.serialize(), &cipher)
                    .map_err(Error::MessageDecryptionFailed)?;
                let msg: T = bincode::deserialize(&plain).map_err(Error::BincodeDeserialize)?;
                Ok((msg, true))
            }
        }
    }

    pub fn plain_or_error(&self) -> Result<&T> {
        match self {
            MaybeEncrypted::Plain(msg) => Ok(msg),
            MaybeEncrypted::Encrypted(_) => Err(Error::UnexpectedEncryptedData),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_custom_message_encrypt_decrypt() {
        let key = SecretKey::random();
        let pubkey = key.pubkey();

        let msg = Message::custom("hello".as_bytes(), Some(pubkey)).unwrap();

        let (plain, is_decrypted) = match msg {
            Message::CustomMessage(cipher) => cipher.decrypt(key).unwrap(),
            _ => panic!("Unexpected message type"),
        };

        assert_eq!(plain, CustomMessage("hello".as_bytes().to_vec()));
        assert!(is_decrypted);
    }
}
