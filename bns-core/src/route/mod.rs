#[cfg(not(feature = "wasm"))]
use crate::channels::default::AcChannel as Channel;
#[cfg(feature = "wasm")]
use crate::channels::wasm::CbChannel as Channel;
use crate::dht::chord::ChordAction;
use crate::did::Did;
use crate::route::message::{Message, PredecessorNotify};
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::Events;
use dashmap::DashMap;
use num_bigint::BigUint;
use rand::Rng;
use std::sync::Arc;

pub mod handler;
pub mod message;

pub type RequestId = u128;

pub struct Routing {
    pub current: Did,
    pub successor: Did,
    pub predecessor: Option<Did>,
    pub fix_finger_index: u8,
    records: DashMap<RequestId, Message>,
    signaler: Arc<Channel>,
    finger_tables: Vec<Option<Did>>,
}

impl Routing {
    pub fn new(ch: Arc<Channel>, current: Did) -> Self {
        return Self {
            current,
            signaler: ch,
            predecessor: None,
            successor: current,
            finger_tables: vec![],
            records: DashMap::new(),
            fix_finger_index: 0,
        };
    }

    fn join_successor(&mut self, successor: Did) -> ChordAction {
        if successor == self.current {
            return ChordAction::None;
        }
        for k in 0u32..160u32 {
            // (n + 2^k) % 2^m >= n
            // pos >= id
            // from n to n + 2^160
            let pos = self.current + Did::from(BigUint::from(2u16).pow(k));
            // pos less than id or id is on another side of ring
            if pos <= successor || pos >= -successor {
                match self.finger_tables[k as usize] {
                    Some(v) => {
                        // for a existed value v
                        // if id < v, then it's more close to this range
                        if successor < v || successor > -v {
                            self.finger_tables[k as usize] = Some(successor);
                            // if id is more close to successor
                        }
                    }
                    None => {
                        self.finger_tables[k as usize] = Some(successor);
                    }
                }
            }
        }
        if (successor - self.current) < (successor - self.successor)
            || self.current == self.successor
        {
            // 1) id should follows self.current
            // 2) #fff should follow #001 because id space is a Finate Ring
            // 3) #001 - #fff = #001 + -(#fff) = #001
            self.successor = successor;
        }

        ChordAction::FindSuccessor((self.successor, self.current))
    }

    pub async fn notify_predecessor(&mut self) {
        if let Some(predecessor) = self.predecessor {
            let mut rng = rand::thread_rng();
            let requestId: RequestId = rng.gen();
            if predecessor > self.current && predecessor < self.successor {
                let message = Message::from(PredecessorNotify {
                    requestId: requestId,
                    current: self.current.clone(),
                    successor: self.successor.clone(),
                });
                self.records.insert(requestId, message.clone());
                match Events::try_from(message) {
                    Ok(event) => self.signaler.send(event).await.unwrap(),
                    Err(e) => {
                        log::error!("Generate events from message `PredecessorNotify` failed");
                    }
                }
            }
        }
    }
}
