use crate::dht::{vnode::VirtualNode, Did};
use crate::ecc::elgamal;
use crate::ecc::{PublicKey, SecretKey};
use crate::err::{Error, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct ConnectNodeSend {
    pub sender_id: Did,
    pub target_id: Did,
    pub transport_uuid: String,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct AlreadyConnected {
    pub answer_id: Did,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct ConnectNodeReport {
    pub answer_id: Did,
    pub transport_uuid: String,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct FindSuccessorSend {
    pub id: Did,
    pub for_fix: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FindSuccessorReport {
    pub id: Did,
    pub for_fix: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct NotifyPredecessorSend {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct NotifyPredecessorReport {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct JoinDHT {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct LeaveDHT {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SearchVNode {
    pub id: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FoundVNode {
    pub data: Vec<VirtualNode>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct StoreVNode {
    pub data: Vec<VirtualNode>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct MultiCall {
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SyncVNodeWithSuccessor {
    pub data: Vec<VirtualNode>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct CustomMessage(pub String);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub enum MaybeEncrypted<T> {
    Encrypted(Vec<(PublicKey, PublicKey)>),
    Plain(T),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub enum Message {
    None,
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
    CustomMessage(MaybeEncrypted<CustomMessage>),
}

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Message {
    pub fn custom(msg: &str, pubkey: &Option<PublicKey>) -> Result<Message> {
        let data = CustomMessage(msg.to_string());
        let msg = MaybeEncrypted::new(data, pubkey)?;
        Ok(Message::CustomMessage(msg))
    }
}

impl<T> MaybeEncrypted<T>
where
    T: Serialize + DeserializeOwned,
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

        let msg = Message::custom("hello", &Some(pubkey)).unwrap();

        let (plain, is_decrypted) = match msg {
            Message::CustomMessage(cipher) => cipher.decrypt(&key).unwrap(),
            _ => panic!("Unexpected message type"),
        };

        assert_eq!(plain, CustomMessage("hello".to_string()));
        assert!(is_decrypted);
    }
}
