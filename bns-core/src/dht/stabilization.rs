use crate::dht::{Chord, ChordAction, ChordRemoteAction};
use crate::err::Result;
use crate::message::{FindSuccessor, Message, MessageRelay, MessageRelayMethod, NotifyPredecessor};
use crate::swarm::Swarm;
use futures::lock::Mutex;
use std::sync::Arc;

pub struct Stabilization {
    chord: Arc<Mutex<Chord>>,
    swarm: Arc<Swarm>,
    timeout: usize,
}

impl Stabilization {
    pub fn new(chord: Arc<Mutex<Chord>>, swarm: Arc<Swarm>, timeout: usize) -> Self {
        Self {
            chord,
            swarm,
            timeout,
        }
    }

    pub fn get_timeout(&self) -> usize {
        self.timeout
    }

    pub async fn stabilize(&self) -> Result<()> {
        let mut chord = self.chord.lock().await;
        let message = MessageRelay::new(
            Message::NotifyPredecessor(NotifyPredecessor {
                id: chord.id,
            }),
            &self.swarm.key,
            None,
            None,
            None,
            MessageRelayMethod::SEND,
        )?;
        if chord.id != chord.successor {
            self.swarm
                .send_message(&chord.successor.into(), message)
                .await?;
        }
        // fix fingers
        match chord.fix_fingers() {
            Ok(action) => match action {
                ChordAction::None => {
                    log::info!("wait to next round");
                    Ok(())
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
                    self.swarm.send_message(&next.into(), message).await
                }
                _ => {
                    log::error!("Invalid Chord Action");
                    unreachable!();
                }
            },
            Err(e) => {
                log::error!("{:?}", e);
                Err(e)
            }
        }
    }
}
