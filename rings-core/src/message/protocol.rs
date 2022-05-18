use crate::dht::Did;

use super::payload::MessageRelay;
use super::payload::MessageRelayMethod;
use super::types::Message;

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
    fn sender(&self) -> Did;
    fn origin(&self) -> Did;
    fn flip(&self) -> Self;
    fn has_next(&self) -> bool;
    fn next(&self) -> Option<Did>;
    fn target(&self) -> Did;
    // add self to list
    fn relay(&mut self, current: Did, next: Option<Did>);

    fn find_prev(&self) -> Option<Did>;
    fn push_prev(&mut self, current: Did, prev: Did);
    fn next_hop(&mut self, current: Did, next: Did);
    fn add_to_path(&mut self, node: Did);
    fn add_from_path(&mut self, node: Did);
    fn remove_to_path(&mut self) -> Option<Did>;
    fn remove_from_path(&mut self) -> Option<Did>;
}

impl MessageSessionRelayProtocol for MessageRelay<Message> {
    // record self to relay list
    // for Send, push self to back of from_path
    // From<A, B> - [Current] - To<C, D> =>
    // From<A, B, Current>, To<C, D>
    // for Report, push self to front of to_path
    // From<A, B> - [Current] - To<C, D> =>
    // From<A, B>, To<Current, C, D>

    fn relay(&mut self, current: Did, next: Option<Did>) {
        match self.method {
            MessageRelayMethod::SEND => {
                if let Some(id) = next {
                    // if next hop is not in path plan, push it to `to_path`
                    if self.to_path.front() != Some(&id) {
                        self.to_path.push_front(id);
                    }
                }
                // always reocrd current; even it's a loop
                self.to_path.pop_back().unwrap();
                self.from_path.push_back(current);
            }
            MessageRelayMethod::REPORT => {
                // should always has a prev
                let prev = self.from_path.pop_back().unwrap();
                self.to_path.push_front(prev);
            }
        }
    }

    // for Send, the last ele of from_path is Sender
    // for Report, the first ele of to_path is Sender
    // A recived Relay should *ALWAYS* has it's sender
    fn sender(&self) -> Did {
        match self.method {
            MessageRelayMethod::SEND => *self.from_path.back().unwrap(),
            MessageRelayMethod::REPORT => *self.to_path.front().unwrap(),
        }
    }

    // Origin is where the msg is send_from
    // for Send it's the first ele of from_path
    // for Report, it's the last ele of to_path
    // A recived Relay should *ALWAYS* has it's origin
    fn origin(&self) -> Did {
        match self.method {
            MessageRelayMethod::SEND => *self.from_path.front().unwrap(),
            MessageRelayMethod::REPORT => *self.to_path.front().unwrap(),
        }
    }

    // A recived Relay should *ALWAYS* has it's target
    fn target(&self) -> Did {
        match self.method {
            MessageRelayMethod::SEND => *self.to_path.back().unwrap(),
            MessageRelayMethod::REPORT => *self.from_path.back().unwrap(),
        }
    }

    #[inline]
    fn flip(&self) -> Self {
        let mut ret = self.clone();
        ret.method = self.method.flip();
        ret
    }

    fn has_next(&self) -> bool {
        match self.method {
            MessageRelayMethod::SEND => !self.to_path.is_empty(),
            MessageRelayMethod::REPORT => !self.from_path.is_empty(),
        }
    }

    // for send, the next hop is the first ele of to_path
    // From<[A, B]> [Current] To<[D, E]> -> D
    // for report, the next hop is the back ele of from_path
    // From<[A, B]> [Current] To<[D, E]> -> D

    fn next(&self) -> Option<Did> {
        match self.method {
            MessageRelayMethod::SEND => self.to_path.front().copied(),
            MessageRelayMethod::REPORT => self.from_path.back().copied(),
        }
    }

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
        }
    }

    #[inline]
    fn push_prev(&mut self, _current: Did, prev: Did) {
        match self.method {
            MessageRelayMethod::SEND => {
                self.from_path.push_back(prev);
            }
            MessageRelayMethod::REPORT => {
                self.to_path.pop_back();
                self.from_path.push_back(prev);
            }
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
