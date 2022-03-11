use crate::message::Did;
use anyhow::anyhow;
use anyhow::Result;
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
        if self.from_path.len() > 0 {
            Some(self.from_path[self.from_path.len() - 1])
        } else {
            None
        }
    }

    #[inline]
    pub fn next_hop(&mut self, current: Did, next: Did) {
        match self.protocol {
            RelayProtocol::SEND => {
                self.to_path.push_back(next);
                self.from_path.push_back(current);
            }
            _ => {}
        };
    }

    pub fn new(
        tx_id: String,
        message_id: String,
        to_path: VecDeque<Did>,
        from_path: VecDeque<Did>,
        protocol: RelayProtocol,
    ) -> Self {
        return Self {
            tx_id,
            message_id,
            to_path,
            from_path,
            protocol,
        };
    }
}
