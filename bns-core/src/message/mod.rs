pub mod handler;

mod encoder;
pub use encoder::Encoded;

mod payload;
pub use payload::MessageRelay;
pub use payload::MessageRelayMethod;

use crate::dht::Did;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct ConnectNode {
    pub sender_id: Did,
    pub target_id: Did,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct AlreadyConnected {
    pub answer_id: Did,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct ConnectedNode {
    pub answer_id: Did,
    pub handshake_info: String,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub struct FindSuccessor {
    id: Did,
    for_fix: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FoundSuccessor {
    successor: Did,
    for_fix: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NotifyPredecessor {
    pub predecessor: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NotifiedPredecessor {
    pub predecessor: Did,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JoinDHT {
    pub id: Did,
}

// A -> B -> C
// B handle_find_success relay with SEND contains
// {
//     from_path: [A]
//     to_path: [B]
// }
// if find successor on B, then return relay with REPORT
// then A get relay contains
// {
//     from_path: [B],
//     to_path: [A]
// }
// otherwise, send to C with relay with SEND contains
// {
//    from_path: [A, B]
//    to_path: [B, C]
// }
// if find successor on C, than return relay with REPORT
// then B get relay contains
// {
//    from_path: [B, C]
//    to_path: [A, B]
// }
pub trait MessageSessionRelayProtocol {
    fn find_prev(&self) -> Option<Did>;
    fn push_prev(&mut self, current: Did, prev: Did);
    fn next_hop(&mut self, current: Did, next: Did);
    fn add_to_path(&mut self, node: Did);
    fn add_from_path(&mut self, node: Did);
    fn remove_to_path(&mut self) -> Option<Did>;
    fn remove_from_path(&mut self) -> Option<Did>;
}

impl MessageSessionRelayProtocol for MessageRelay<Message> {
    #[inline]
    fn find_prev(&self) -> Option<Did> {
        match self.method {
            MessageRelayMethod::SEND => {
                if !self.from_path.is_empty() {
                    self.from_path.back().cloned()
                } else {
                    None
                }
            }
            MessageRelayMethod::REPORT => {
                if !self.to_path.is_empty() {
                    self.to_path.back().cloned()
                } else {
                    None
                }
            }
            _ => unreachable!(),
        }
    }

    #[inline]
    fn push_prev(&mut self, current: Did, prev: Did) {
        match self.method {
            MessageRelayMethod::SEND => {
                self.from_path.push_back(prev);
            }
            MessageRelayMethod::REPORT => {
                assert_eq!(self.to_path.pop_back(), Some(current));
                self.to_path.pop_back();
                self.from_path.push_back(prev);
            }
            _ => unreachable!(),
        }
    }

    #[inline]
    fn next_hop(&mut self, current: Did, next: Did) {
        match self.method {
            MessageRelayMethod::SEND => {
                self.to_path.push_back(next);
                self.from_path.push_back(current);
            }
            MessageRelayMethod::REPORT => unimplemented!(),
            _ => unreachable!(),
        };
    }

    #[inline]
    fn add_to_path(&mut self, node: Did) {
        self.to_path.push_back(node);
    }

    #[inline]
    fn add_from_path(&mut self, node: Did) {
        self.from_path.push_back(node);
    }

    #[inline]
    fn remove_to_path(&mut self) -> Option<Did> {
        if !self.to_path.is_empty() {
            self.to_path.pop_back()
        } else {
            None
        }
    }

    #[inline]
    fn remove_from_path(&mut self) -> Option<Did> {
        if !self.from_path.is_empty() {
            self.from_path.pop_back()
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum Message {
    None,
    JoinDHT(JoinDHT),
    ConnectNode(ConnectNode),
    AlreadyConnected(AlreadyConnected),
    ConnectedNode(ConnectedNode),
    FindSuccessor(FindSuccessor),
    FoundSuccessor(FoundSuccessor),
    NotifyPredecessor(NotifyPredecessor),
    NotifiedPredecessor(NotifiedPredecessor),
}
