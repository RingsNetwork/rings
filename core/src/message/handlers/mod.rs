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
use crate::error::Error;
use crate::error::Result;
use crate::hooks::BoxedMessageCallback;
use crate::hooks::BoxedMessageValidator;
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
    AcceptAnswer(NextHop, ConnectNodeReport),

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
    /// CallbackFn implement `customMessage` and `builtin_message`.
    callback: Option<Arc<BoxedMessageCallback>>,
    /// A specific validator implement ValidatorFn.
    validator: Option<Arc<BoxedMessageValidator>>,
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
    pub fn new(
        dht: Arc<PeerRing>,
        callback: Option<BoxedMessageCallback>,
        validator: Option<BoxedMessageValidator>,
    ) -> Self {
        Self {
            dht,
            callback: callback.map(Arc::new),
            validator: validator.map(Arc::new),
        }
    }

    /// Invoke callback, which will be call after builtin handler.
    async fn invoke_callback(&self, payload: &MessagePayload<Message>) -> Vec<MessageHandlerEvent> {
        if let Some(ref cb) = self.callback {
            match payload.data {
                Message::CustomMessage(ref msg) => {
                    if self.dht.did == payload.relay.destination {
                        tracing::debug!("INVOKE CUSTOM MESSAGE CALLBACK {}", &payload.tx_id);
                        return cb.custom_message(payload, msg).await;
                    }
                }
                _ => return cb.builtin_message(payload).await,
            };
        } else if let Message::CustomMessage(ref msg) = payload.data {
            if self.dht.did == payload.relay.destination {
                tracing::warn!("No callback registered, skip invoke_callback of {:?}", msg);
            }
        }
        vec![]
    }

    /// Validate message.
    async fn validate(&self, payload: &MessagePayload<Message>) -> Result<()> {
        if let Some(ref v) = self.validator {
            v.validate(payload)
                .await
                .map(|info| Err(Error::InvalidMessage(info)))
                .unwrap_or(Ok(()))?;
        };
        Ok(())
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

        self.validate(payload).await?;

        let mut events = match &payload.data {
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

        tracing::debug!("INVOKE CALLBACK {}", &payload.tx_id);

        events.extend(self.invoke_callback(payload).await);

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
    use crate::hooks::MessageCallback;
    use crate::message::CustomMessage;
    use crate::message::PayloadSender;
    use crate::swarm::Swarm;
    use crate::tests::default::prepare_node_with_callback;
    use crate::tests::manually_establish_connection;

    #[derive(Clone)]
    struct MessageCallbackInstance {
        #[allow(clippy::type_complexity)]
        handler_messages: Arc<Mutex<Vec<(Did, Vec<u8>)>>>,
    }

    #[tokio::test]
    async fn test_custom_message_handling() -> Result<()> {
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();

        #[async_trait]
        impl MessageCallback for MessageCallbackInstance {
            async fn custom_message(
                &self,
                ctx: &MessagePayload<Message>,
                msg: &CustomMessage,
            ) -> Vec<MessageHandlerEvent> {
                self.handler_messages
                    .lock()
                    .await
                    .push((ctx.addr, msg.0.clone()));
                println!("{:?}, {:?}, {:?}", ctx, ctx.addr, msg);
                vec![]
            }

            async fn builtin_message(
                &self,
                ctx: &MessagePayload<Message>,
            ) -> Vec<MessageHandlerEvent> {
                println!("{:?}, {:?}", ctx, ctx.addr);
                vec![]
            }
        }

        let msg_callback1 = MessageCallbackInstance {
            handler_messages: Arc::new(Mutex::new(vec![])),
        };
        let msg_callback2 = MessageCallbackInstance {
            handler_messages: Arc::new(Mutex::new(vec![])),
        };

        let (node1, _path1) =
            prepare_node_with_callback(key1, Some(msg_callback1.clone().boxed())).await;
        let (node2, _path2) =
            prepare_node_with_callback(key2, Some(msg_callback2.clone().boxed())).await;

        manually_establish_connection(&node1, &node2).await?;

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

        assert_eq!(msg_callback1.handler_messages.lock().await.as_slice(), &[
            (node2.did(), "Hello world 2 to 1 - 1".as_bytes().to_vec()),
            (node2.did(), "Hello world 2 to 1 - 2".as_bytes().to_vec())
        ]);

        assert_eq!(msg_callback2.handler_messages.lock().await.as_slice(), &[
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
