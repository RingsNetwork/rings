#![warn(missing_docs)]
//! This module implemented message handler of rings network.
/// Message Flow:
/// +---------+    +--------------------------------+
/// | Message | -> | MessageHandler.handler_payload |
/// +---------+    +--------------------------------+
///                 ||                            ||
///     +--------------------------+  +--------------------------+
///     | Builtin Message Callback |  |  Custom Message Callback |
///     +--------------------------+  +--------------------------+
use std::sync::Arc;

use async_recursion::async_recursion;
use async_trait::async_trait;

use super::Message;
use super::MessagePayload;
use crate::dht::vnode::VirtualNode;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::error::Result;
use crate::message::ConnectNodeReport;
use crate::message::ConnectNodeSend;

/// Operator and Handler for Connection
pub mod connection;
/// Operator and Handler for CustomMessage
pub mod custom;
/// For handle dht related actions
pub mod dht;
/// Operator and handler for DHT stablization
pub mod stabilization;
/// Operator and Handler for Storage
pub mod storage;
/// Operator and Handler for Subring
pub mod subring;

/// Type alias for message payload.
pub type Payload = MessagePayload<Message>;

type NextHop = Did;

/// MessageHandlerEvent that will be handled by Swarm.
#[derive(Debug, Clone)]
pub enum MessageHandlerEvent {
    /// Instructs the swarm to connect to a peer.
    Connect(Did),
    /// Instructs the swarm to connect to a peer via given next hop.
    ConnectVia(Did, NextHop),
    /// Instructs the swarm to disconnect from a peer.
    Disconnect(Did),

    /// Instructs the swarm to answer an offer inside payload by given
    /// sender's Did and Message.
    AnswerOffer(Payload, ConnectNodeSend),

    /// Instructs the swarm to accept an answer inside payload by given
    /// sender's Did and Message.
    AcceptAnswer(Did, ConnectNodeReport),

    /// Tell swarm to forward the payload to destination by given
    /// Payload and optional next hop.
    ForwardPayload(Payload, Option<Did>),

    /// Instructs the swarm to notify the dht about new peer.
    JoinDHT(Payload, Did),

    /// Instructs the swarm to send a direct message to a peer.
    SendDirectMessage(Message, Did),

    /// Instructs the swarm to send a message to a peer via the dht network.
    SendMessage(Message, Did),

    /// Instructs the swarm to send a message as a response to the received message.
    SendReportMessage(Payload, Message),

    /// Instructs the swarm to send a message to a peer via the dht network with a specific next hop.
    ResetDestination(Payload, Did),

    /// Instructs the swarm to store vnode.
    StorageStore(VirtualNode),
    /// Notify a node
    Notify(Did),
}

/// MessageHandler will manage resources.
#[derive(Clone)]
pub struct MessageHandler {
    dht: Arc<PeerRing>,
}

/// Generic trait for handle message ,inspired by Actor-Model.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait HandleMsg<T> {
    /// Message handler.
    async fn handle(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &T,
    ) -> Result<Vec<MessageHandlerEvent>>;
}

impl MessageHandler {
    /// Create a new MessageHandler Instance.
    pub fn new(dht: Arc<PeerRing>) -> Self {
        Self { dht }
    }

    /// Handle builtin message.
    #[cfg_attr(feature = "wasm", async_recursion(?Send))]
    #[cfg_attr(not(feature = "wasm"), async_recursion)]
    pub async fn handle_message(
        &self,
        payload: &MessagePayload<Message>,
    ) -> Result<Vec<MessageHandlerEvent>> {
        #[cfg(test)]
        {
            println!("{} got msg {}", self.dht.did, &payload.data);
        }
        tracing::debug!("START HANDLE MESSAGE: {} {}", &payload.tx_id, &payload.data);

        let events = match &payload.data {
            Message::JoinDHT(ref msg) => self.handle(payload, msg).await,
            Message::LeaveDHT(ref msg) => self.handle(payload, msg).await,
            Message::ConnectNodeSend(ref msg) => self.handle(payload, msg).await,
            Message::ConnectNodeReport(ref msg) => self.handle(payload, msg).await,
            Message::FindSuccessorSend(ref msg) => self.handle(payload, msg).await,
            Message::FindSuccessorReport(ref msg) => self.handle(payload, msg).await,
            Message::NotifyPredecessorSend(ref msg) => self.handle(payload, msg).await,
            Message::NotifyPredecessorReport(ref msg) => self.handle(payload, msg).await,
            Message::SearchVNode(ref msg) => self.handle(payload, msg).await,
            Message::FoundVNode(ref msg) => self.handle(payload, msg).await,
            Message::SyncVNodeWithSuccessor(ref msg) => self.handle(payload, msg).await,
            Message::OperateVNode(ref msg) => self.handle(payload, msg).await,
            Message::CustomMessage(ref msg) => self.handle(payload, msg).await,
            Message::QueryForTopoInfoSend(ref msg) => self.handle(payload, msg).await,
            Message::QueryForTopoInfoReport(ref msg) => self.handle(payload, msg).await,
        }?;

        tracing::debug!("FINISH HANDLE MESSAGE {}", &payload.tx_id);
        Ok(events)
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
pub mod tests {
    use futures::lock::Mutex;
    use tokio::time::sleep;
    use tokio::time::Duration;

    use super::*;
    use crate::dht::Did;
    use crate::ecc::SecretKey;
    use crate::message::PayloadSender;
    use crate::swarm::callback::SwarmCallback;
    use crate::swarm::Swarm;
    use crate::tests::default::prepare_node_with_callback;
    use crate::tests::manually_establish_connection;

    #[derive(Clone)]
    struct CallbackInstance {
        #[allow(clippy::type_complexity)]
        handler_messages: Arc<Mutex<Vec<(Did, Vec<u8>)>>>,
    }

    #[tokio::test]
    async fn test_custom_message_handling() -> Result<()> {
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();

        #[async_trait]
        impl SwarmCallback for CallbackInstance {
            async fn on_payload(
                &self,
                payload: &MessagePayload<Message>,
            ) -> std::result::Result<(), Box<dyn std::error::Error>> {
                println!("{:?}, {:?}, {:?}", payload, payload.addr, payload.data);

                if let Message::CustomMessage(ref msg) = payload.data {
                    self.handler_messages
                        .lock()
                        .await
                        .push((payload.addr, msg.0.clone()))
                }

                Ok(())
            }
        }

        let cb1 = CallbackInstance {
            handler_messages: Arc::new(Mutex::new(vec![])),
        };

        let cb2 = CallbackInstance {
            handler_messages: Arc::new(Mutex::new(vec![])),
        };

        let (node1, _path1) = prepare_node_with_callback(key1, Some(cb1.clone().boxed())).await;
        let (node2, _path2) = prepare_node_with_callback(key2, Some(cb2.clone().boxed())).await;

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
        loop {
            tokio::select! {
                _ = node1.listen_once() => {}
                _ = node2.listen_once() => {}
                _ = node3.listen_once() => {}
                _ = sleep(Duration::from_secs(3)) => break
            }
        }
    }
}
