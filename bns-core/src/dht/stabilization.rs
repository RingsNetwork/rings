use crate::dht::{Chord, ChordAction, ChordRemoteAction};
use crate::message::{FindSuccessor, Message, MessageRelay, MessageRelayMethod, NotifyPredecessor};
use crate::swarm::Swarm;
use anyhow::Result;
use futures::lock::Mutex;
use std::sync::Arc;

pub struct Stabilization {
    chord: Arc<Mutex<Chord>>,
    swarm: Arc<Swarm>,
}

impl Stabilization {
    pub fn new(chord: Arc<Mutex<Chord>>, swarm: Arc<Swarm>) -> Self {
        Self { chord, swarm }
    }

    pub async fn stabilize(&self) -> Result<()> {
        loop {
            let mut chord = self.chord.lock().await;
            let message = MessageRelay::new(
                Message::NotifyPredecessor(NotifyPredecessor {
                    predecessor: chord.id,
                }),
                &self.swarm.key,
                None,
                None,
                None,
                MessageRelayMethod::SEND,
            )?;
            self.swarm.send_message(&chord.id.into(), message).await?;
            // fix fingers
            match chord.fix_fingers() {
                Ok(action) => match action {
                    ChordAction::None => {
                        log::info!("wait to next round");
                    }
                    ChordAction::RemoteAction(
                        next,
                        ChordRemoteAction::FindSuccessorForFix(current),
                    ) => {
                        let message = MessageRelay::new(
                            Message::FindSuccessor(FindSuccessor {
                                id: current,
                                for_fix: true,
                            }),
                            &self.swarm.key,
                            None,
                            None,
                            None,
                            MessageRelayMethod::SEND,
                        )?;
                        self.swarm.send_message(&next.into(), message).await?;
                    }
                    _ => {
                        log::error!("Invalid Chord Action");
                    }
                },
                Err(e) => log::error!("{:?}", e),
            }
        }
    }
}
