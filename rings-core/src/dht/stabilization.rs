use crate::dht::{ChordStablize, PeerRing, PeerRingAction, PeerRingRemoteAction};
use crate::err::Result;
use crate::message::{
    FindSuccessorSend, Message, MessageRelay, MessageRelayMethod, NotifyPredecessorSend,
    OriginVerificationGen,
};
use crate::swarm::Swarm;

use async_trait::async_trait;
use futures::lock::Mutex;
use std::sync::Arc;

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
        let message = MessageRelay::new(
            Message::NotifyPredecessorSend(NotifyPredecessorSend { id: chord.id }),
            &self.swarm.session_manager,
            OriginVerificationGen::Origin,
            MessageRelayMethod::SEND,
            None,
            None,
            None,
            self.swarm.address().into(),
        )?;
        if chord.id != chord.successor.min() {
            for s in chord.successor.list() {
                self.swarm.send_message(&s.into(), message.clone()).await?;
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
                    let message = MessageRelay::new(
                        Message::FindSuccessorSend(FindSuccessorSend {
                            id: current,
                            for_fix: true,
                        }),
                        &self.swarm.session_manager,
                        OriginVerificationGen::Origin,
                        MessageRelayMethod::SEND,
                        None,
                        None,
                        None,
                        self.swarm.address().into(),
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

#[cfg(not(feature = "wasm"))]
mod stabilizer {
    use super::{Stabilization, TStabilize};
    use async_trait::async_trait;
    use futures::{future::FutureExt, pin_mut, select};
    use futures_timer::Delay;
    use std::sync::Arc;
    use std::time::Duration;

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
    use super::{Stabilization, TStabilize};
    use crate::poll;
    use async_trait::async_trait;
    use std::sync::Arc;
    use wasm_bindgen_futures::spawn_local;

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
