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

impl Routing {
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
