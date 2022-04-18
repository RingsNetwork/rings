use crate::dht::{PeerRing, PeerRingAction, PeerRingRemoteAction, ChordStablize};
use crate::err::Result;
use crate::message::{FindSuccessor, Message, MessageRelay, MessageRelayMethod, NotifyPredecessor};
use crate::swarm::Swarm;
use futures::lock::Mutex;
use std::sync::Arc;

pub struct Stabilization {
    chord: Arc<Mutex<PeerRing>>,
    swarm: Arc<Swarm>,
    timeout: usize,
}

impl Stabilization {
    pub fn new(chord: Arc<Mutex<PeerRing>>, swarm: Arc<Swarm>, timeout: usize) -> Self {
        Self {
            chord,
            swarm,
            timeout,
        }
    }

    pub fn get_timeout(&self) -> usize {
        self.timeout
    }

    async fn notify_predecessor(&self) -> Result<()> {
        let chord = self.chord.lock().await;
        let message = MessageRelay::new(
            Message::NotifyPredecessor(NotifyPredecessor { id: chord.id }),
            &self.swarm.session(),
            None,
            None,
            None,
            MessageRelayMethod::SEND,
        )?;
        if chord.id != chord.successor {
            self.swarm
                .send_message(&chord.successor.into(), message)
                .await
        } else {
            Ok(())
        }
    }

    async fn fix_fingers(&self) -> Result<()> {
        let mut chord = self.chord.lock().await;
        match chord.fix_fingers() {
            Ok(action) => match action {
                PeerRingAction::None => {
                    log::info!("wait to next round");
                    Ok(())
                }
                PeerRingAction::RemoteAction(
                    next,
                    PeerRingRemoteAction::FindSuccessorForFix(current),
                ) => {
                    let message = MessageRelay::new(
                        Message::FindSuccessor(FindSuccessor {
                            id: current,
                            for_fix: true,
                        }),
                        &self.swarm.session(),
                        None,
                        None,
                        None,
                        MessageRelayMethod::SEND,
                    )?;
                    self.swarm.send_message(&next.into(), message).await
                }
                _ => {
                    log::error!("Invalid PeerRing Action");
                    unreachable!();
                }
            },
            Err(e) => {
                log::error!("{:?}", e);
                Err(e)
            }
        }
    }

    pub async fn stabilize(&self) -> Result<()> {
        self.notify_predecessor().await?;
        self.fix_fingers().await?;
        Ok(())
    }
}
