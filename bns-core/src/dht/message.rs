use crate::channels::default::AcChannel;
use crate::dht::chord::Chord;
use crate::dht::chord::ChordAction;
use crate::types::channel::Events;
use serde_json;
use std::sync::{Arc, Mutex as StdMutex};
use webrtc::data_channel::RTCDataChannel;

#[derive(Clone, Debug)]
pub struct Procedures {
    pub routing: Arc<StdMutex<Chord>>,
}

impl Procedures {
    pub fn new(buffer: usize, routing: Chord) -> Self {
        Self {
            routing: Arc::new(StdMutex::new(routing)),
        }
    }

    pub async fn send_message(channel: Arc<RTCDataChannel>, message: Vec<u8>) {
        channel.send(&message.into()).await;
    }

    pub fn notify_predecessor(&self, signaler: Arc<AcChannel>, msg: ChordAction) {}

    pub fn find_peer() {}

    pub fn notify_and_join(&self, msg: Vec<u8>) {
        // Deserialize msg and get ChordAction, join successor
    }
}
