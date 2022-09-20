use std::sync::Arc;

use async_trait::async_trait;

use crate::dht::ChordStabilize;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::err::Result;
use crate::message::FindSuccessorSend;
use crate::message::FindSuccessorThen;
use crate::message::FindSuccessorAnd;
use crate::message::Message;
use crate::message::NotifyPredecessorSend;
use crate::message::PayloadSender;
use crate::swarm::Swarm;

#[derive(Clone)]
pub struct Stabilization {
    chord: Arc<PeerRing>,
    swarm: Arc<Swarm>,
    timeout: usize,
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TStabilize {
    async fn wait(self: Arc<Self>);
}

impl Stabilization {
    pub fn new(swarm: Arc<Swarm>, timeout: usize) -> Self {
        Self {
            chord: swarm.dht(),
            swarm,
            timeout,
        }
    }

    pub fn get_timeout(&self) -> usize {
        self.timeout
    }

    async fn notify_predecessor(&self) -> Result<()> {
        let (successor_min, successor_list) = {
            let successor = self.chord.lock_successor()?;
            (successor.min(), successor.list())
        };

        let msg = Message::NotifyPredecessorSend(NotifyPredecessorSend { id: self.chord.id });
        if self.chord.id != successor_min {
            for s in successor_list {
                self.swarm
                    .send_message(msg.clone(), s, self.swarm.did())
                    .await?;
            }
            Ok(())
        } else {
            Ok(())
        }
    }

    async fn fix_fingers(&self) -> Result<()> {
        match self.chord.fix_fingers() {
            Ok(action) => match action {
                PeerRingAction::None => {
                    // log::debug!("wait to next round");
                    Ok(())
                }
                PeerRingAction::RemoteAction(
                    next,
                    PeerRingRemoteAction::FindSuccessorForFix(current),
                ) => {
                    let msg = Message::FindSuccessorSend(FindSuccessorSend {
                        id: current,
                        and: FindSuccessorAnd::Report,
                        strict: true,
                        report_then: FindSuccessorThen::FixFingerTable,
                    });
                    self.swarm
                        .send_message(msg.clone(), next, self.swarm.did())
                        .await
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
        if let Err(e) = self.notify_predecessor().await {
            log::error!("[stabilize] Failed on notify predecessor {:?}", e);
        }
        if let Err(e) = self.fix_fingers().await {
            log::error!("[stabilize] Failed on fix_finger {:?}", e);
        }
        Ok(())
    }
}

#[cfg(not(feature = "wasm"))]
mod stabilizer {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use futures::future::FutureExt;
    use futures::pin_mut;
    use futures::select;
    use futures_timer::Delay;

    use super::Stabilization;
    use super::TStabilize;

    #[async_trait]
    impl TStabilize for Stabilization {
        async fn wait(self: Arc<Self>) {
            loop {
                let timeout = Delay::new(Duration::from_secs(self.timeout as u64)).fuse();
                pin_mut!(timeout);
                select! {
                    _ = timeout => self
                        .stabilize()
                        .await
                        .unwrap_or_else(|e| log::error!("failed to stabilize {:?}", e)),
                }
            }
        }
    }
}

#[cfg(feature = "wasm")]
mod stabilizer {
    use std::sync::Arc;

    use async_trait::async_trait;
    use wasm_bindgen_futures::spawn_local;

    use super::Stabilization;
    use super::TStabilize;
    use crate::poll;

    #[async_trait(?Send)]
    impl TStabilize for Stabilization {
        async fn wait(self: Arc<Self>) {
            let caller = Arc::clone(&self);
            let func = move || {
                let caller = caller.clone();
                spawn_local(Box::pin(async move {
                    caller
                        .stabilize()
                        .await
                        .unwrap_or_else(|e| log::error!("failed to stabilize {:?}", e));
                }))
            };
            poll!(func, 25000);
        }
    }
}
