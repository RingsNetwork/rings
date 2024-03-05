#![warn(missing_docs)]
//! This module implemented message handler of rings network.

use std::sync::Arc;

use async_recursion::async_recursion;
use async_trait::async_trait;

use super::MessagePayload;
use crate::dht::Chord;
use crate::dht::CorrectChord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::error::Error;
use crate::error::Result;
use crate::message::types::FindSuccessorSend;
use crate::message::types::Message;
use crate::message::types::QueryForTopoInfoSend;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorThen;
use crate::message::NotifyPredecessorSend;
use crate::message::PayloadSender;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::callback::SharedSwarmCallback;
use crate::transport::SwarmTransport;

/// Operator and Handler for Connection
pub mod connection;
/// Operator and Handler for CustomMessage
pub mod custom;
/// Operator and handler for DHT stablization
pub mod stabilization;
/// Operator and Handler for Storage
pub mod storage;
/// Operator and Handler for Subring
pub mod subring;

/// MessageHandler will manage resources.
#[derive(Clone)]
pub struct MessageHandler {
    transport: Arc<SwarmTransport>,
    dht: Arc<PeerRing>,
    swarm_callback: SharedSwarmCallback,
}

/// Generic trait for handle message ,inspired by Actor-Model.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait HandleMsg<T> {
    /// Message handler.
    async fn handle(&self, ctx: &MessagePayload, msg: &T) -> Result<()>;
}

impl MessageHandler {
    /// Create a new MessageHandler instance.
    pub fn new(transport: Arc<SwarmTransport>, swarm_callback: SharedSwarmCallback) -> Self {
        let dht = transport.dht.clone();
        Self {
            transport,
            dht,
            swarm_callback,
        }
    }

    fn inner_callback(&self) -> InnerSwarmCallback {
        InnerSwarmCallback::new(self.transport.clone(), self.swarm_callback.clone())
    }

    pub(crate) async fn join_dht(&self, peer: Did) -> Result<()> {
        if cfg!(feature = "experimental") {
            let conn = self
                .transport
                .get_connection(peer)
                .ok_or(Error::SwarmMissDidInTable(peer))?;
            let dht_ev = self.dht.join_then_sync(conn).await?;
            self.handle_dht_events(&dht_ev).await
        } else {
            let dht_ev = self.dht.join(peer)?;
            self.handle_dht_events(&dht_ev).await
        }
    }

    pub(crate) async fn leave_dht(&self, peer: Did) -> Result<()> {
        if self
            .transport
            .get_and_check_connection(peer)
            .await
            .is_none()
        {
            self.dht.remove(peer)?
        };
        Ok(())
    }

    #[cfg_attr(feature = "wasm", async_recursion(?Send))]
    #[cfg_attr(not(feature = "wasm"), async_recursion)]
    pub(crate) async fn handle_dht_events(&self, act: &PeerRingAction) -> Result<()> {
        match act {
            PeerRingAction::None => Ok(()),
            // Ask next hop to find successor for did,
            // if there is only two nodes A, B, it may cause loop, for example
            // A's successor is B, B ask A to find successor for B
            // A may send message to it's successor, which is B
            PeerRingAction::RemoteAction(
                next,
                PeerRingRemoteAction::FindSuccessorForConnect(did),
            ) => {
                if next != did {
                    self.transport
                        .send_direct_message(
                            Message::FindSuccessorSend(FindSuccessorSend {
                                did: *did,
                                strict: false,
                                then: FindSuccessorThen::Report(
                                    FindSuccessorReportHandler::Connect,
                                ),
                            }),
                            *next,
                        )
                        .await?;
                    Ok(())
                } else {
                    Ok(())
                }
            }
            // A new successor is set, request the new successor for it's successor list
            PeerRingAction::RemoteAction(next, PeerRingRemoteAction::QueryForSuccessorList) => {
                self.transport
                    .send_direct_message(
                        Message::QueryForTopoInfoSend(QueryForTopoInfoSend::new_for_sync(*next)),
                        *next,
                    )
                    .await?;
                Ok(())
            }
            PeerRingAction::RemoteAction(did, PeerRingRemoteAction::TryConnect) => {
                self.transport.connect(*did, self.inner_callback()).await?;
                Ok(())
            }
            PeerRingAction::RemoteAction(did, PeerRingRemoteAction::Notify(target_id)) => {
                if did == target_id {
                    tracing::warn!("Did is equal to target_id, may implement wrong.");
                    return Ok(());
                }
                let msg =
                    Message::NotifyPredecessorSend(NotifyPredecessorSend { did: self.dht.did });
                self.transport.send_message(msg, *target_id).await?;
                Ok(())
            }
            PeerRingAction::MultiActions(acts) => {
                let jobs = acts
                    .iter()
                    .map(|act| async move { self.handle_dht_events(act).await });

                for res in futures::future::join_all(jobs).await {
                    if res.is_err() {
                        tracing::error!("Failed on handle multi actions: {:#?}", res)
                    }
                }

                Ok(())
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
pub mod tests {
    use dashmap::DashMap;
    use futures::lock::Mutex;
    use tokio::time::sleep;
    use tokio::time::Duration;

    use super::*;
    use crate::dht::Did;
    use crate::ecc::SecretKey;
    use crate::message::MessageVerificationExt;
    use crate::message::PayloadSender;
    use crate::swarm::callback::SwarmCallback;
    use crate::swarm::Swarm;
    use crate::tests::default::prepare_node;
    use crate::tests::manually_establish_connection;

    struct SwarmCallbackInstance {
        handler_messages: Mutex<Vec<(Did, Vec<u8>)>>,
    }

    #[tokio::test]
    async fn test_custom_message_handling() -> Result<()> {
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();

        #[async_trait]
        impl SwarmCallback for SwarmCallbackInstance {
            async fn on_inbound(
                &self,
                payload: &MessagePayload,
            ) -> std::result::Result<(), Box<dyn std::error::Error>> {
                let msg: Message = payload.transaction.data().map_err(Box::new)?;

                match msg {
                    Message::CustomMessage(ref msg) => {
                        self.handler_messages
                            .lock()
                            .await
                            .push((payload.transaction.signer(), msg.0.clone()));
                        println!("{:?}, {:?}, {:?}", payload, payload.signer(), msg);
                    }
                    _ => {
                        println!("{:?}, {:?}", payload, payload.signer());
                    }
                }

                Ok(())
            }
        }

        let cb1 = Arc::new(SwarmCallbackInstance {
            handler_messages: Mutex::new(vec![]),
        });
        let cb2 = Arc::new(SwarmCallbackInstance {
            handler_messages: Mutex::new(vec![]),
        });

        let node1 = prepare_node(key1).await;
        let node2 = prepare_node(key2).await;

        node1.set_callback(cb1.clone()).unwrap();
        node2.set_callback(cb2.clone()).unwrap();

        manually_establish_connection(&node1, &node2).await;

        let node11 = node1.clone();
        let node22 = node2.clone();
        tokio::spawn(async move { node11.listen().await });
        tokio::spawn(async move { node22.listen().await });

        println!("waiting for data channel ready");
        sleep(Duration::from_secs(5)).await;

        println!("sending messages");
        node1
            .send_message(
                Message::custom("Hello world 1 to 2 - 1".as_bytes())?,
                node2.did(),
            )
            .await
            .unwrap();

        node1
            .send_message(
                Message::custom("Hello world 1 to 2 - 2".as_bytes())?,
                node2.did(),
            )
            .await?;

        node2
            .send_message(
                Message::custom("Hello world 2 to 1 - 1".as_bytes())?,
                node1.did(),
            )
            .await?;

        node1
            .send_message(
                Message::custom("Hello world 1 to 2 - 3".as_bytes())?,
                node2.did(),
            )
            .await?;

        node2
            .send_message(
                Message::custom("Hello world 2 to 1 - 2".as_bytes())?,
                node1.did(),
            )
            .await?;

        sleep(Duration::from_secs(5)).await;

        assert_eq!(cb1.handler_messages.lock().await.as_slice(), &[
            (node2.did(), "Hello world 2 to 1 - 1".as_bytes().to_vec()),
            (node2.did(), "Hello world 2 to 1 - 2".as_bytes().to_vec())
        ]);

        assert_eq!(cb2.handler_messages.lock().await.as_slice(), &[
            (node1.did(), "Hello world 1 to 2 - 1".as_bytes().to_vec()),
            (node1.did(), "Hello world 1 to 2 - 2".as_bytes().to_vec()),
            (node1.did(), "Hello world 1 to 2 - 3".as_bytes().to_vec())
        ]);

        Ok(())
    }

    pub async fn assert_no_more_msg(node1: &Swarm, node2: &Swarm, node3: &Swarm) {
        tokio::select! {
            _ = node1.listen_once() => unreachable!("node1 should not receive any message"),
            _ = node2.listen_once() => unreachable!("node2 should not receive any message"),
            _ = node3.listen_once() => unreachable!("node3 should not receive any message"),
            _ = sleep(Duration::from_secs(3)) => {}
        }
    }

    pub async fn wait_for_msgs(node1: &Swarm, node2: &Swarm, node3: &Swarm) {
        let did_names: DashMap<Did, &str, _> = DashMap::new();
        did_names.insert(node1.did(), "node1");
        did_names.insert(node2.did(), "node2");
        did_names.insert(node3.did(), "node3");

        let listen1 = async {
            loop {
                tokio::select! {
                    Some((payload, _)) = node1.listen_once() => {
                        println!(
                            "Msg {} => node1 : {:?}",
                            *did_names.get(&payload.signer()).unwrap(),
                            payload.transaction.data::<Message>().unwrap()
                        )
                    }
                    _ = sleep(Duration::from_secs(3)) => break
                }
            }
        };

        let listen2 = async {
            loop {
                tokio::select! {
                    Some((payload, _)) = node2.listen_once() => {
                        println!(
                            "Msg {} => node2 : {:?}",
                            *did_names.get(&payload.signer()).unwrap(),
                            payload.transaction.data::<Message>().unwrap()
                        )
                    }
                    _ = sleep(Duration::from_secs(3)) => break
                }
            }
        };

        let listen3 = async {
            loop {
                tokio::select! {
                    Some((payload, _)) = node3.listen_once() => {
                        println!(
                            "Msg {} => node3 : {:?}",
                            *did_names.get(&payload.signer()).unwrap(),
                            payload.transaction.data::<Message>().unwrap()
                        )
                    }
                    _ = sleep(Duration::from_secs(3)) => break
                }
            }
        };

        futures::join!(listen1, listen2, listen3);
    }
}
