use crate::message::Did;
use serde::Deserialize;
use serde::Serialize;
use std::collections::VecDeque;

#[derive(Debug, PartialEq, Clone, Deserialize, Serialize)]
pub enum RelayProtocol {
    SEND,
    REPORT,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessageRelay {
    pub tx_id: String,
    pub message_id: String,
    pub to_path: VecDeque<Did>,
    pub from_path: VecDeque<Did>,
    pub protocol: RelayProtocol,
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

impl MessageRelay {
    #[inline]
    pub fn find_prev(&self) -> Option<Did> {
        match self.protocol {
            RelayProtocol::SEND => {
                if !self.from_path.is_empty() {
                    Some(self.from_path[self.from_path.len() - 1])
                } else {
                    None
                }
            }
            RelayProtocol::REPORT => {
                if !self.to_path.is_empty() {
                    Some(self.to_path[self.to_path.len() - 1])
                } else {
                    None
                }
            }
        }
    }

    #[inline]
    pub fn next_hop(&mut self, current: Did, next: Did) {
        match self.protocol {
            RelayProtocol::SEND => {
                self.to_path.push_back(next);
                self.from_path.push_back(current);
            }
            RelayProtocol::REPORT => unimplemented!(),
        };
    }

    #[inline]
    pub fn add_to_path(&mut self, node: Did) {
        self.to_path.push_back(node);
    }

    #[inline]
    pub fn add_from_path(&mut self, node: Did) {
        self.from_path.push_back(node);
    }

    #[inline]
    pub fn remove_to_path(&mut self) -> Option<Did> {
        if !self.to_path.is_empty() {
            self.to_path.pop_back()
        } else {
            None
        }
    }

    #[inline]
    pub fn remove_from_path(&mut self) -> Option<Did> {
        if !self.from_path.is_empty() {
            self.from_path.pop_back()
        } else {
            None
        }
    }

    pub fn new_with_path(
        tx_id: String,
        message_id: String,
        to_path: VecDeque<Did>,
        from_path: VecDeque<Did>,
        protocol: RelayProtocol,
    ) -> Self {
        Self {
            tx_id,
            message_id,
            to_path,
            from_path,
            protocol,
        }
    }

    pub fn new_with_node(
        tx_id: String,
        message_id: String,
        next: Did,
        current: Did,
        protocol: RelayProtocol,
    ) -> Self {
        let mut to_path = VecDeque::new();
        let mut from_path = VecDeque::new();
        to_path.push_back(next);
        from_path.push_back(current);
        Self {
            tx_id,
            message_id,
            to_path,
            from_path,
            protocol,
        }
    }

    pub fn get_send_relay(
        relay: &MessageRelay,
        tx_id: String,
        message_id: String,
        prev_node: Did,
        next_node: Did,
    ) -> Self {
        match relay.protocol {
            RelayProtocol::SEND => {
                let mut from_path = relay.from_path.clone();
                let mut to_path = relay.to_path.clone();
                from_path.push_back(prev_node);
                to_path.push_back(next_node);
                MessageRelay::new_with_path(
                    tx_id,
                    message_id,
                    from_path,
                    to_path,
                    RelayProtocol::SEND,
                )
            }
            RelayProtocol::REPORT => {
                unimplemented!()
            }
        }
    }

    pub fn get_report_relay(relay: &MessageRelay, tx_id: String, message_id: String) -> Self {
        match relay.protocol {
            RelayProtocol::REPORT => MessageRelay::new_with_path(
                tx_id,
                message_id,
                relay.from_path.clone(),
                relay.to_path.clone(),
                RelayProtocol::REPORT,
            ),
            RelayProtocol::SEND => {
                unimplemented!();
            }
        }
    }
}
