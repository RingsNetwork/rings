use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::vnode::VirtualNode;
use crate::dht::Did;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::err::Error;
use crate::err::Result;

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
/// MessageType use to ask for connection, send to remote with transport_uuid and handshake_info.
pub struct ConnectNodeSend {
    pub transport_uuid: String,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
/// MessageType report to origin that already connected.
pub struct AlreadyConnected;

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
/// MessageType report to origin with own transport_uuid and handshake_info.
pub struct ConnectNodeReport {
    pub transport_uuid: String,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
/// MessageType use to find successor in a chord ring.
pub struct FindSuccessorSend {
    pub id: Did,
    pub strict: bool,
    pub then: FindSuccessorThen,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType use to report origin node with report message.
pub struct FindSuccessorReport {
    pub id: Did,
    pub handler: FindSuccessorReportHandler,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType use to notify predecessor, ask for update finger tables.
pub struct NotifyPredecessorSend {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType report to origin node.
pub struct NotifyPredecessorReport {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType use to join chord ring, add did into fingers table.
pub struct JoinDHT {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType use to leave chord ring.
pub struct LeaveDHT {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType use to search virtual node.
pub struct SearchVNode {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType report to origin found virtual node.
pub struct FoundVNode {
    pub data: Vec<VirtualNode>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType ask to store data/message into virtual node.
pub struct StoreVNode {
    pub data: Vec<VirtualNode>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType contains multi messages.
pub struct MultiCall {
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType after `FindSuccessorSend` and syncing data.
pub struct SyncVNodeWithSuccessor {
    pub data: Vec<VirtualNode>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType use to join virtual node in subring.
pub struct JoinSubRing {
    pub did: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// MessageType use to customize message, will be handle by `custom_message` method.
pub struct CustomMessage(pub Vec<u8>);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
/// A enum about Encrypted and Plain types.
pub enum MaybeEncrypted<T> {
    Encrypted(Vec<u8>),
    Plain(T),
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
/// MessageType enum Report contain FindSuccessorSend.
pub enum FindSuccessorThen {
    Report(FindSuccessorReportHandler),
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
/// MessageType enum handle when meet the last node.
/// - None: do nothing but return.
/// - Connect: connect origin node.
/// - FixFingerTable: update fingers table.
/// - SyncStorage: syncing data in virtual node.
/// - CustomCallback: custom callback handle by `custom_message` method.
pub enum FindSuccessorReportHandler {
    None,
    Connect,
    FixFingerTable,
    SyncStorage,
    CustomCallback(u8),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[non_exhaustive]
/// A collection MessageType use for unified management.
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
