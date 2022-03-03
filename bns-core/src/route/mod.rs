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
use lazy_static::lazy_static;
use num_bigint::BigUint;
use rand::Rng;
use std::sync::Arc;
use std::sync::Mutex;

pub mod handler;
pub mod message;

pub type RequestID = u128;

lazy_static! {
    pub(crate) static ref routing: Mutex<Routing> = Mutex::new(Routing::new());
}

pub struct Routing {
    pub current: Option<Did>,
    pub successor: Option<Did>,
    pub predecessor: Option<Did>,
    pub fix_finger_index: u8,
    records: DashMap<RequestID, Message>,
    finger_tables: Vec<Option<Did>>,
}

impl Routing {
    pub fn new() -> Self {
        Self {
            current: None,
            predecessor: None,
            successor: None,
            finger_tables: vec![],
            records: DashMap::new(),
            fix_finger_index: 0,
        }
    }

    pub fn set_current(&mut self, current: Did) {
        self.current = Some(current);
    }

    pub fn set_successor(&mut self, successor: Did) {
        self.successor = Some(successor);
    }

    pub fn join_successor(&mut self, successor: Did) -> ChordAction {
        let current = self.current.unwrap();
        if Some(successor) == self.current {
            return ChordAction::None;
        } else if self.current.is_none() && self.successor.is_none() {
            return ChordAction::None;
        }
        for k in 0u32..160u32 {
            // (n + 2^k) % 2^m >= n
            // pos >= id
            // from n to n + 2^160
            let pos = current + Did::from(BigUint::from(2u16).pow(k));
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
        if (successor - current) < (successor - successor) || self.current == self.successor {
            // 1) id should follows self.current
            // 2) #fff should follow #001 because id space is a Finate Ring
            // 3) #001 - #fff = #001 + -(#fff) = #001
            self.set_successor(successor);
        }
        if self.successor.is_none() {
            ChordAction::None;
        }
        ChordAction::FindSuccessor((self.successor.unwrap(), self.current.unwrap()))
    }

    pub async fn notify_predecessor(&mut self, signaler: Arc<Channel>) {
        let current = self.current.unwrap();
        let successor = self.successor.unwrap();
        if let Some(predecessor) = self.predecessor {
            let mut rng = rand::thread_rng();
            let request_id: RequestID = rng.gen();
            if predecessor > current && predecessor < successor {
                let message = Message::from(PredecessorNotify {
                    request_id: request_id,
                    current: current,
                    successor: successor,
                });
                self.records.insert(request_id, message.clone());
                match Events::try_from(message) {
                    Ok(event) => signaler.send(event).await.unwrap(),
                    Err(e) => {
                        log::error!("Generate events from message `PredecessorNotify` failed");
                    }
                }
            }
        }
    }
}
