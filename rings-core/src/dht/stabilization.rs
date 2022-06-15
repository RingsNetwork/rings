use std::sync::Arc;

use async_trait::async_trait;
use futures::lock::Mutex;

use crate::dht::ChordStablize;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::err::Result;
use crate::message::FindSuccessorSend;
use crate::message::Message;
use crate::message::NotifyPredecessorSend;
use crate::message::PayloadSender;
use crate::swarm::Swarm;

#[derive(Clone)]
pub struct Stabilization {
    chord: Arc<Mutex<PeerRing>>,
    swarm: Arc<Swarm>,
    timeout: usize,
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TStabilize {
    async fn wait(self: Arc<Self>);
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
        let msg = Message::NotifyPredecessorSend(NotifyPredecessorSend { id: chord.id });
        if chord.id != chord.successor.min() {
            for s in chord.successor.list() {
                self.swarm
                    .send_message(msg.clone(), s, self.swarm.address().into())
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
                    log::debug!("wait to next round");
                    Ok(())
                }
                PeerRingAction::RemoteAction(
                    next,
                    PeerRingRemoteAction::FindSuccessorForFix(current),
                ) => {
                    let msg = Message::FindSuccessorSend(FindSuccessorSend {
                        id: current,
                        for_fix: true,
                    });
                    self.swarm
                        .send_message(msg.clone(), next, self.swarm.address().into())
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
        self.notify_predecessor().await?;
        self.fix_fingers().await?;
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
                    _ = timeout => {
                        match self.stabilize().await {
                            Ok(()) => {},
                            Err(e) => {
                                log::error!("failed to stabilize {:?}", e);
                            }
                        };
                    }
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
                    caller.stabilize().await.unwrap();
                }))
            };
            poll!(func, 200);
        }
    }
}
