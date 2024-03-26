//! Stabilization run daemons to maintain dht.

use std::sync::Arc;

use rings_transport::core::transport::WebrtcConnectionState;

use crate::dht::successor::SuccessorReader;
use crate::dht::types::CorrectChord;
use crate::dht::Chord;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::error::Result;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorSend;
use crate::message::FindSuccessorThen;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::NotifyPredecessorSend;
use crate::message::PayloadSender;
use crate::message::QueryForTopoInfoSend;
use crate::transport::SwarmTransport;

/// The stabilization runner.
#[derive(Clone)]
pub struct Stabilizer {
    transport: Arc<SwarmTransport>,
    dht: Arc<PeerRing>,
}

impl Stabilizer {
    /// Create a new stabilization runner.
    pub fn new(transport: Arc<SwarmTransport>) -> Self {
        let dht = transport.dht.clone();
        Self { transport, dht }
    }

    /// Run stabilization once.
    pub async fn stabilize(&self) -> Result<()> {
        tracing::debug!("STABILIZATION notify_predecessor start");
        if let Err(e) = self.notify_predecessor().await {
            tracing::error!("[stabilize] Failed on notify predecessor {:?}", e);
        }
        tracing::debug!("STABILIZATION notify_predecessor end");
        tracing::debug!("STABILIZATION fix_fingers start");
        if let Err(e) = self.fix_fingers().await {
            tracing::error!("[stabilize] Failed on fix_finger {:?}", e);
        }
        tracing::debug!("STABILIZATION fix_fingers end");
        tracing::debug!("STABILIZATION clean_unavailable_connections start");
        if let Err(e) = self.clean_unavailable_connections().await {
            tracing::error!(
                "[stabilize] Failed on clean unavailable connections {:?}",
                e
            );
        }
        tracing::debug!("STABILIZATION clean_unavailable_connections end");
        #[cfg(feature = "experimental")]
        {
            tracing::debug!("STABILIZATION correct_stabilize start");
            if let Err(e) = self.correct_stabilize().await {
                tracing::error!("[stabilize] Failed on call correct stabilize {:?}", e);
            }
            tracing::debug!("STABILIZATION correct_stabilize end");
        }
        Ok(())
    }

    /// Clean unavailable connections in transport.
    pub async fn clean_unavailable_connections(&self) -> Result<()> {
        let conns = self.transport.get_connections();

        for (did, conn) in conns.into_iter() {
            if matches!(
                conn.webrtc_connection_state(),
                WebrtcConnectionState::Disconnected
                    | WebrtcConnectionState::Failed
                    | WebrtcConnectionState::Closed
            ) {
                tracing::info!("STABILIZATION clean_unavailable_transports: {:?}", did);
                self.transport.disconnect(did).await?;
            }
        }

        Ok(())
    }

    /// Notify predecessor, this is a DHT operation.
    pub async fn notify_predecessor(&self) -> Result<()> {
        let (successor_min, successor_list) = {
            let successor = self.dht.successors();
            (successor.min()?, successor.list()?)
        };

        let msg = Message::NotifyPredecessorSend(NotifyPredecessorSend { did: self.dht.did });
        if self.dht.did != successor_min {
            for s in successor_list {
                tracing::debug!("STABILIZATION notify_predecessor: {:?}", s);
                let payload = MessagePayload::new_send(
                    msg.clone(),
                    self.transport.session_sk(),
                    s,
                    self.dht.did,
                )?;
                self.transport.send_payload(payload).await?;
            }
            Ok(())
        } else {
            Ok(())
        }
    }

    /// Fix fingers from finger table, this is a DHT operation.
    async fn fix_fingers(&self) -> Result<()> {
        match self.dht.fix_fingers() {
            Ok(action) => match action {
                PeerRingAction::None => Ok(()),
                PeerRingAction::RemoteAction(
                    closest_predecessor,
                    PeerRingRemoteAction::FindSuccessorForFix(finger_did),
                ) => {
                    tracing::debug!("STABILIZATION fix_fingers: {:?}", finger_did);
                    let msg = Message::FindSuccessorSend(FindSuccessorSend {
                        did: finger_did,
                        then: FindSuccessorThen::Report(FindSuccessorReportHandler::FixFingerTable),
                        strict: false,
                    });
                    let payload = MessagePayload::new_send(
                        msg.clone(),
                        self.transport.session_sk(),
                        closest_predecessor,
                        closest_predecessor,
                    )?;
                    self.transport.send_payload(payload).await?;
                    Ok(())
                }
                _ => {
                    tracing::error!("Invalid PeerRing Action");
                    unreachable!();
                }
            },
            Err(e) => {
                tracing::error!("{:?}", e);
                Err(e)
            }
        }
    }

    /// Call stabilization from correct chord implementation
    pub async fn correct_stabilize(&self) -> Result<()> {
        if let PeerRingAction::RemoteAction(
            next,
            PeerRingRemoteAction::QueryForSuccessorListAndPred,
        ) = self.dht.pre_stabilize()?
        {
            self.transport
                .send_direct_message(
                    Message::QueryForTopoInfoSend(QueryForTopoInfoSend::new_for_stab(next)),
                    next,
                )
                .await?;
        }
        Ok(())
    }
}

#[cfg(not(feature = "wasm"))]
mod stabilizer {
    use std::sync::Arc;
    use std::time::Duration;

    use futures::future::FutureExt;
    use futures::pin_mut;
    use futures::select;
    use futures_timer::Delay;

    use super::*;

    impl Stabilizer {
        /// Run stabilization in a loop.
        pub async fn wait(self: Arc<Self>, interval: Duration) {
            loop {
                let timeout = Delay::new(interval).fuse();
                pin_mut!(timeout);
                select! {
                    _ = timeout => self
                        .stabilize()
                        .await
                        .unwrap_or_else(|e| tracing::error!("failed to stabilize {:?}", e)),
                }
            }
        }
    }
}

#[cfg(feature = "wasm")]
mod stabilizer {
    use std::sync::Arc;
    use std::time::Duration;

    use wasm_bindgen_futures::spawn_local;

    use super::*;
    use crate::poll;

    impl Stabilizer {
        /// Run stabilization in a loop.
        pub async fn wait(self: Arc<Self>, interval: Duration) {
            let caller = Arc::clone(&self);
            let func = move || {
                let caller = caller.clone();
                spawn_local(Box::pin(async move {
                    caller
                        .stabilize()
                        .await
                        .unwrap_or_else(|e| tracing::error!("failed to stabilize {:?}", e));
                }))
            };
            poll!(func, interval.as_millis().try_into().unwrap());
        }
    }
}
