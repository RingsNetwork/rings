use crate::dht::{ChordStablize, PeerRing, PeerRingAction, PeerRingRemoteAction};
use crate::err::Result;
use crate::message::{
    FindSuccessorSend, Message, MessageRelay, MessageRelayMethod, NotifyPredecessorSend,
};
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
            Message::NotifyPredecessorSend(NotifyPredecessorSend { id: chord.id }),
            &self.swarm.session(),
            None,
            None,
            None,
            MessageRelayMethod::SEND,
        )?;
        if chord.id != chord.successor.min() {
            for s in chord.successor.list() {
                self.swarm
                    .send_message(&s.into(), message.clone())
                    .await?;
            }
            Ok(())
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
                        Message::FindSuccessorSend(FindSuccessorSend {
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
