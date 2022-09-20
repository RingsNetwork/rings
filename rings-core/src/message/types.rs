use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::vnode::VirtualNode;
use crate::dht::Did;
use crate::ecc::elgamal;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::err::Error;
use crate::err::Result;

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct ConnectNodeSend {
    pub transport_uuid: String,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct AlreadyConnected;

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct ConnectNodeReport {
    pub transport_uuid: String,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct FindSuccessorSend {
    pub id: Did,
    pub strict: bool,
    pub and: FindSuccessorAnd,
    pub report_then: FindSuccessorThen,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FindSuccessorReport {
    pub id: Did,
    pub then: FindSuccessorThen,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RequestServiceReport {
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NotifyPredecessorSend {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NotifyPredecessorReport {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JoinDHT {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LeaveDHT {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SearchVNode {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FoundVNode {
    pub data: Vec<VirtualNode>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct StoreVNode {
    pub data: Vec<VirtualNode>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct MultiCall {
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SyncVNodeWithSuccessor {
    pub data: Vec<VirtualNode>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JoinSubRing {
    pub did: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CustomMessage(pub Vec<u8>);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub enum MaybeEncrypted<T> {
    Encrypted(Vec<(PublicKey, PublicKey)>),
    Plain(T),
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FindSuccessorThen {
    None,
    Connect,
    FixFingerTable,
    SyncStorage,
    CustomCallback(u8),
}
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FindSuccessorAnd {
    None,
    Report,
    RequestService(Vec<u8>),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub enum Message {
    MultiCall(MultiCall),
    JoinDHT(JoinDHT),
    LeaveDHT(LeaveDHT),
    ConnectNodeSend(ConnectNodeSend),
    AlreadyConnected(AlreadyConnected),
    ConnectNodeReport(ConnectNodeReport),
    RequestServiceReport(RequestServiceReport),
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
    pub fn custom(msg: &[u8], pubkey: &Option<PublicKey>) -> Result<Message> {
        let data = CustomMessage(msg.to_vec());
        let msg = MaybeEncrypted::new(data, pubkey)?;
        Ok(Message::CustomMessage(msg))
    }
}

impl<T> MaybeEncrypted<T>
where T: Serialize + DeserializeOwned
{
    pub fn new(data: T, pubkey: &Option<PublicKey>) -> Result<Self> {
        if let Some(pubkey) = pubkey {
            let msg = serde_json::to_string(&data).map_err(Error::Serialize)?;
            let cipher = elgamal::encrypt(&msg, pubkey)?;
            Ok(MaybeEncrypted::Encrypted(cipher))
        } else {
            Ok(MaybeEncrypted::Plain(data))
        }
    }

    pub fn decrypt(self, key: &SecretKey) -> Result<(T, bool)> {
        match self {
            MaybeEncrypted::Plain(msg) => Ok((msg, false)),
            MaybeEncrypted::Encrypted(cipher) => {
                let plain = elgamal::decrypt(&cipher, key)?;
                let msg: T = serde_json::from_str(&plain).map_err(Error::Serialize)?;
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

        let msg = Message::custom("hello".as_bytes(), &Some(pubkey)).unwrap();

        let (plain, is_decrypted) = match msg {
            Message::CustomMessage(cipher) => cipher.decrypt(&key).unwrap(),
            _ => panic!("Unexpected message type"),
        };

        assert_eq!(plain, CustomMessage("hello".as_bytes().to_vec()));
        assert!(is_decrypted);
    }
}
