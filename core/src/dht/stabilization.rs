//! Stabilization wait to notify predecessors and update fingersTable.
use std::sync::Arc;

use async_trait::async_trait;

use crate::dht::successor::SuccessorReader;
use crate::dht::types::CorrectChord;
use crate::dht::Chord;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::error::Result;
use crate::message::handlers::MessageHandlerEvent;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorSend;
use crate::message::FindSuccessorThen;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::NotifyPredecessorSend;
use crate::message::PayloadSender;
use crate::message::QueryForTopoInfoSend;
use crate::swarm::Swarm;
use crate::transports::manager::TransportManager;
use crate::types::ice_transport::IceTransportInterface;

/// A combination contains chord and swarm, use to run stabilize.
/// - swarm: transports communicate with each others.
/// - chord: fix local fingers table.
#[derive(Clone)]
pub struct Stabilization {
    chord: Arc<PeerRing>,
    swarm: Arc<Swarm>,
    timeout: usize,
}

/// A trait with `wait` method.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TStabilize {
    /// Wait and poll
    async fn wait(self: Arc<Self>);
}

impl Stabilization {
    /// Clean unavailable transports from swarm.
    pub async fn clean_unavailable_transports(&self) -> Result<()> {
        let transports = self.swarm.get_transports();

        for (did, t) in transports.into_iter() {
            if t.is_disconnected().await {
                tracing::info!("STABILIZATION clean_unavailable_transports: {:?}", did);
                self.swarm.disconnect(did).await?;
            }
        }

        Ok(())
    }
}

impl Stabilization {
    /// Create a new instance of Stabilization
    pub fn new(swarm: Arc<Swarm>, timeout: usize) -> Self {
        Self {
            chord: swarm.dht(),
            swarm,
            timeout,
        }
    }

    /// Get timeout of waiting delays.
    pub fn get_timeout(&self) -> usize {
        self.timeout
    }
}

impl Stabilization {
    /// Notify predecessor, this is a DHT operation.
    pub async fn notify_predecessor(&self) -> Result<()> {
        let (successor_min, successor_list) = {
            let successor = self.chord.successors();
            (successor.min()?, successor.list()?)
        };

        let msg = Message::NotifyPredecessorSend(NotifyPredecessorSend {
            did: self.chord.did,
        });
        if self.chord.did != successor_min {
            for s in successor_list {
                tracing::debug!("STABILIZATION notify_predecessor: {:?}", s);
                let payload = MessagePayload::new_send(
                    msg.clone(),
                    self.swarm.delegatee_sk(),
                    s,
                    self.swarm.did(),
                )?;
                self.swarm.send_payload(payload).await?;
            }
            Ok(())
        } else {
            Ok(())
        }
    }

    /// Fix fingers from finger table, this is a DHT operation.
    async fn fix_fingers(&self) -> Result<()> {
        match self.chord.fix_fingers() {
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
                        self.swarm.delegatee_sk(),
                        closest_predecessor,
                        closest_predecessor,
                    )?;
                    self.swarm.send_payload(payload).await?;
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
}

impl Stabilization {
    /// Call stabilization from correct chord implementation
    pub async fn correct_stabilize(&self) -> Result<()> {
        if let PeerRingAction::RemoteAction(
            next,
            PeerRingRemoteAction::QueryForSuccessorListAndPred,
        ) = self.chord.pre_stabilize()?
        {
            let evs = vec![MessageHandlerEvent::SendDirectMessage(
                Message::QueryForTopoInfoSend(QueryForTopoInfoSend::new_for_stab(next)),
                next,
            )];
            return self.swarm.handle_message_handler_events(&evs).await;
        }
        Ok(())
    }
}

impl Stabilization {
    /// Call stabilize periodly.
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
        tracing::debug!("STABILIZATION clean_unavailable_transports start");
        if let Err(e) = self.clean_unavailable_transports().await {
            tracing::error!("[stabilize] Failed on clean unavailable transports {:?}", e);
        }
        tracing::debug!("STABILIZATION clean_unavailable_transports end");
        #[cfg(feature = "experimental")]
        {
            tracing::debug!("STABILIZATION correct_stabilize start");
            if let Err(e) = self.correct_stabilize() {
                tracing::error!("[stabilize] Failed on call correct stabilize {:?}", e);
            }
            tracing::debug!("STABILIZATION correct_stabilize end");
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
                        .unwrap_or_else(|e| tracing::error!("failed to stabilize {:?}", e)),
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
                        .unwrap_or_else(|e| tracing::error!("failed to stabilize {:?}", e));
                }))
            };
            poll!(func, 25000);
        }
    }
}

#[cfg(not(any(feature = "wasm", feature = "dummy")))]
#[cfg(test)]
pub mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ecc::SecretKey;
    use crate::prelude::RTCIceConnectionState;
    use crate::tests::default::prepare_node;
    use crate::tests::manually_establish_connection;

    #[tokio::test]
    async fn test_clean_unavailable_transports() {
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();
        let key3 = SecretKey::random();
        let (node1, _) = prepare_node(key1).await;
        let (node2, _) = prepare_node(key2).await;
        let (node3, _) = prepare_node(key3).await;

        // Shouldn't listen to message handler here,
        // otherwise it will automatically remove disconnected transport.

        manually_establish_connection(&node1, &node2).await.unwrap();
        manually_establish_connection(&node1, &node3).await.unwrap();

        tokio::time::sleep(Duration::from_secs(5)).await;

        assert_eq!(
            node1
                .get_transport(node2.did())
                .unwrap()
                .ice_connection_state()
                .await
                .unwrap(),
            RTCIceConnectionState::Connected
        );
        assert_eq!(
            node1
                .get_transport(node3.did())
                .unwrap()
                .ice_connection_state()
                .await
                .unwrap(),
            RTCIceConnectionState::Connected
        );

        node2.disconnect(node1.did()).await.unwrap();
        node3.disconnect(node1.did()).await.unwrap();

        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(
            node1
                .get_transport(node2.did())
                .unwrap()
                .ice_connection_state()
                .await
                .unwrap(),
            RTCIceConnectionState::Disconnected
        );
        assert_eq!(
            node1
                .get_transport(node3.did())
                .unwrap()
                .ice_connection_state()
                .await
                .unwrap(),
            RTCIceConnectionState::Disconnected
        );

        let stb = Stabilization::new(node1.clone(), 3);
        stb.clean_unavailable_transports().await.unwrap();

        assert!(node1.get_transport(node2.did()).is_none());
        assert!(node1.get_transport(node3.did()).is_none());
    }
}
