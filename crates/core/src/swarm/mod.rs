#![warn(missing_docs)]
//! Tranposrt management

mod builder;
/// Callback interface for swarm
pub mod callback;
/// Implementations of connection management traits for swarm
pub mod impls;
mod types;

use std::sync::Arc;
use std::sync::RwLock;

use async_recursion::async_recursion;
use async_trait::async_trait;
pub use builder::SwarmBuilder;
use rings_derive::JudgeConnection;
use rings_transport::core::transport::BoxedTransport;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportMessage;
use rings_transport::core::transport::WebrtcConnectionState;
use rings_transport::error::Error as TransportError;
pub use types::MeasureImpl;
pub use types::WrappedDid;

use crate::channels::Channel;
use crate::chunk::ChunkList;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::consts::TRANSPORT_MTU;
use crate::dht::types::Chord;
use crate::dht::CorrectChord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::error::Error;
use crate::error::Result;
use crate::inspect::SwarmInspect;
use crate::message;
use crate::message::types::NotifyPredecessorSend;
use crate::message::ChordStorageInterface;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessageHandlerEvent;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::message::PayloadSender;
use crate::session::SessionSk;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::impls::ConnectionHandshake;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::channel::TransportEvent;
use crate::types::Connection;
use crate::types::ConnectionOwner;

/// The transport and dht management.
#[derive(JudgeConnection)]
pub struct Swarm {
    /// Event channel for receive events from transport.
    pub(crate) transport_event_channel: Channel<TransportEvent>,
    /// Reference of DHT.
    pub(crate) dht: Arc<PeerRing>,
    /// Implementationof measurement.
    pub(crate) measure: Option<MeasureImpl>,
    session_sk: SessionSk,
    message_handler: MessageHandler,
    transport: BoxedTransport<ConnectionOwner, TransportError>,
    callback: RwLock<SharedSwarmCallback>,
}

impl Swarm {
    /// Get did of self.
    pub fn did(&self) -> Did {
        self.dht.did
    }

    /// Get DHT(Distributed Hash Table) of self.
    pub fn dht(&self) -> Arc<PeerRing> {
        self.dht.clone()
    }

    /// Retrieves the session sk associated with the current instance.
    /// The session sk provides a segregated approach to manage private keys.
    /// It generates session secret keys for the bound entries of PKIs (Public Key Infrastructure).
    pub fn session_sk(&self) -> &SessionSk {
        &self.session_sk
    }

    /// Load message from a TransportEvent.
    async fn load_message(&self, ev: TransportEvent) -> Result<Option<MessagePayload>> {
        match ev {
            TransportEvent::DataChannelMessage(msg) => {
                let payload = MessagePayload::from_bincode(&msg)?;
                tracing::debug!("load message from channel: {:?}", payload);
                Ok(Some(payload))
            }
            TransportEvent::Connected(did) => match self.get_connection(did) {
                Some(_) => {
                    let payload = MessagePayload::new_send(
                        Message::JoinDHT(message::JoinDHT { did }),
                        &self.session_sk,
                        self.dht.did,
                        self.dht.did,
                    )?;
                    Ok(Some(payload))
                }
                None => Err(Error::SwarmMissTransport(did)),
            },
            TransportEvent::Closed(did) => {
                let payload = MessagePayload::new_send(
                    Message::LeaveDHT(message::LeaveDHT { did }),
                    &self.session_sk,
                    self.dht.did,
                    self.dht.did,
                )?;
                Ok(Some(payload))
            }
        }
    }

    /// This method is required because web-sys components is not `Send`
    /// which means an async loop cannot running concurrency.
    pub async fn poll_message(&self) -> Option<MessagePayload> {
        let receiver = &self.transport_event_channel.receiver();
        match Channel::recv(receiver).await {
            Ok(Some(ev)) => match self.load_message(ev).await {
                Ok(Some(msg)) => Some(msg),
                Ok(None) => None,
                Err(_) => None,
            },
            Ok(None) => None,
            Err(e) => {
                tracing::error!("Failed on polling message, Error {}", e);
                None
            }
        }
    }

    /// This method is required because web-sys components is not `Send`
    /// This method will return events already consumed (landed), which is ok to be ignore.
    /// which means a listening loop cannot running concurrency.
    pub async fn listen_once(&self) -> Option<(MessagePayload, Vec<MessageHandlerEvent>)> {
        let payload = self.poll_message().await?;

        if !(payload.verify() && payload.transaction.verify()) {
            tracing::error!("Cannot verify msg or it's expired: {:?}", payload);
            return None;
        }
        let events = self.message_handler.handle_message(&payload).await;

        match events {
            Ok(evs) => {
                self.handle_message_handler_events(&evs)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::error!(
                            "Swarm failed on handling event from message handler: {:#?}",
                            e
                        );
                    });
                Some((payload, evs))
            }
            Err(e) => {
                tracing::error!("Message handler failed on handling event: {:#?}", e);
                None
            }
        }
    }

    /// Event handler of Swarm.
    pub async fn handle_message_handler_event(
        &self,
        event: &MessageHandlerEvent,
    ) -> Result<Vec<MessageHandlerEvent>> {
        tracing::debug!("Handle message handler event: {:?}", event);
        match event {
            MessageHandlerEvent::Connect(did) => {
                let did = *did;
                if self.get_and_check_connection(did).await.is_none() && did != self.did() {
                    self.connect(did).await?;
                }
                Ok(vec![])
            }

            // Notify did with self.id
            MessageHandlerEvent::Notify(did) => {
                let msg =
                    Message::NotifyPredecessorSend(NotifyPredecessorSend { did: self.dht.did });
                Ok(vec![MessageHandlerEvent::SendMessage(msg, *did)])
            }

            MessageHandlerEvent::ConnectVia(did, next) => {
                let did = *did;
                if self.get_and_check_connection(did).await.is_none() && did != self.did() {
                    self.connect_via(did, *next).await?;
                }
                Ok(vec![])
            }

            MessageHandlerEvent::Disconnect(did) => {
                self.disconnect(*did).await?;
                Ok(vec![])
            }

            MessageHandlerEvent::AnswerOffer(relay, msg) => {
                let (_, answer) = self
                    .answer_remote_connection(relay.relay.origin_sender(), msg)
                    .await?;

                Ok(vec![MessageHandlerEvent::SendReportMessage(
                    relay.clone(),
                    Message::ConnectNodeReport(answer),
                )])
            }

            MessageHandlerEvent::AcceptAnswer(origin_sender, msg) => {
                self.accept_remote_connection(origin_sender.to_owned(), msg)
                    .await?;
                Ok(vec![])
            }

            MessageHandlerEvent::ForwardPayload(payload, next_hop) => {
                if self
                    .get_and_check_connection(payload.relay.destination)
                    .await
                    .is_some()
                {
                    self.forward_payload(payload, Some(payload.relay.destination))
                        .await?;
                } else {
                    self.forward_payload(payload, *next_hop).await?;
                }
                Ok(vec![])
            }

            MessageHandlerEvent::JoinDHT(ctx, did) => {
                if cfg!(feature = "experimental") {
                    let wdid: WrappedDid = WrappedDid::new(self, *did);
                    let dht_ev = self.dht.join_then_sync(wdid).await?;
                    crate::message::handlers::dht::handle_dht_events(&dht_ev, ctx).await
                } else {
                    let dht_ev = self.dht.join(*did)?;
                    crate::message::handlers::dht::handle_dht_events(&dht_ev, ctx).await
                }
            }

            MessageHandlerEvent::SendDirectMessage(msg, dest) => {
                self.send_direct_message(msg.clone(), *dest).await?;
                Ok(vec![])
            }

            MessageHandlerEvent::SendMessage(msg, dest) => {
                self.send_message(msg.clone(), *dest).await?;
                Ok(vec![])
            }

            MessageHandlerEvent::SendReportMessage(payload, msg) => {
                self.send_report_message(payload, msg.clone()).await?;
                Ok(vec![])
            }

            MessageHandlerEvent::ResetDestination(payload, next_hop) => {
                self.reset_destination(payload, *next_hop).await?;
                Ok(vec![])
            }

            MessageHandlerEvent::StorageStore(vnode) => {
                <Self as ChordStorageInterface<1>>::storage_store(self, vnode.clone()).await?;
                Ok(vec![])
            }
        }
    }

    /// Batch handle events
    #[cfg_attr(feature = "wasm", async_recursion(?Send))]
    #[cfg_attr(not(feature = "wasm"), async_recursion)]
    pub async fn handle_message_handler_events(
        &self,
        events: &Vec<MessageHandlerEvent>,
    ) -> Result<()> {
        match events.as_slice() {
            [] => Ok(()),
            [x, xs @ ..] => {
                let evs = self.handle_message_handler_event(x).await?;
                self.handle_message_handler_events(&evs).await?;
                self.handle_message_handler_events(&xs.to_vec()).await
            }
        }
    }

    /// Disconnect a connection. There are three steps:
    /// 1) remove from DHT;
    /// 2) remove from Transport;
    /// 3) close the connection;
    pub async fn disconnect(&self, did: Did) -> Result<()> {
        JudgeConnection::disconnect(self, did).await
    }

    /// Connect a given Did. If the did is already connected, return directly,
    /// else try prepare offer and establish connection by dht.
    /// This function may returns a pending connection or connected connection.
    pub async fn connect(&self, did: Did) -> Result<Connection> {
        JudgeConnection::connect(self, did).await
    }

    /// Similar to connect, but this function will try connect a Did by given hop.
    pub async fn connect_via(&self, did: Did, next_hop: Did) -> Result<Connection> {
        JudgeConnection::connect_via(self, did, next_hop).await
    }

    /// Check the status of swarm
    pub async fn inspect(&self) -> SwarmInspect {
        SwarmInspect::inspect(self).await
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl PayloadSender for Swarm {
    fn session_sk(&self) -> &SessionSk {
        Swarm::session_sk(self)
    }

    fn dht(&self) -> Arc<PeerRing> {
        Swarm::dht(self)
    }

    fn is_connected(&self, did: Did) -> bool {
        let Some(conn) = self.get_connection(did) else {
            return false;
        };
        conn.webrtc_connection_state() == WebrtcConnectionState::Connected
    }

    async fn do_send_payload(&self, did: Did, payload: MessagePayload) -> Result<()> {
        let conn = self
            .get_and_check_connection(did)
            .await
            .ok_or(Error::SwarmMissDidInTable(did))?;

        tracing::debug!(
            "Try send {:?}, to node {:?}",
            payload.clone(),
            payload.relay.next_hop,
        );

        let data = payload.to_bincode()?;
        if data.len() > TRANSPORT_MAX_SIZE {
            tracing::error!("Message is too large: {:?}", payload);
            return Err(Error::MessageTooLarge(data.len()));
        }

        let result = if data.len() > TRANSPORT_MTU {
            let chunks = ChunkList::<TRANSPORT_MTU>::from(&data);
            for chunk in chunks {
                let data =
                    MessagePayload::new_send(Message::Chunk(chunk), &self.session_sk, did, did)?
                        .to_bincode()?;
                conn.send_message(TransportMessage::Custom(data.to_vec()))
                    .await?;
            }
            Ok(())
        } else {
            conn.send_message(TransportMessage::Custom(data.to_vec()))
                .await
        };

        tracing::debug!(
            "Sent {:?}, to node {:?}",
            payload.clone(),
            payload.relay.next_hop,
        );

        if result.is_ok() {
            self.record_sent(payload.relay.next_hop).await
        } else {
            self.record_sent_failed(payload.relay.next_hop).await
        }

        result.map_err(|e| e.into())
    }
}

#[cfg(not(feature = "wasm"))]
impl Swarm {
    /// Listener for native envirement, It will just launch a loop.
    pub async fn listen(self: Arc<Self>) {
        loop {
            self.listen_once().await;
        }
    }
}

#[cfg(feature = "wasm")]
impl Swarm {
    /// Listener for browser envirement, the implementation is based on  js_sys::window.set_timeout.
    pub async fn listen(self: Arc<Self>) {
        let func = move || {
            let this = self.clone();
            wasm_bindgen_futures::spawn_local(Box::pin(async move {
                this.listen_once().await;
            }));
        };
        crate::poll!(func, 10);
    }
}
