use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use futures::lock::Mutex as FuturesMutex;
use rings_transport::connection_ref::ConnectionRef;
#[cfg(feature = "dummy")]
pub use rings_transport::connections::DummyConnection as ConnectionOwner;
#[cfg(feature = "dummy")]
pub use rings_transport::connections::DummyTransport as Transport;
#[cfg(feature = "wasm")]
pub use rings_transport::connections::WebSysWebrtcConnection as ConnectionOwner;
#[cfg(feature = "wasm")]
pub use rings_transport::connections::WebSysWebrtcTransport as Transport;
#[cfg(all(not(feature = "wasm"), not(feature = "dummy")))]
use rings_transport::connections::WebrtcConnection as ConnectionOwner;
#[cfg(all(not(feature = "wasm"), not(feature = "dummy")))]
use rings_transport::connections::WebrtcTransport as Transport;
use rings_transport::core::media::ChannelConfig;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;
use rings_transport::core::transport::TransportMessage;
use rings_transport::core::transport::WebrtcConnectionState;
use rings_transport::delivery::DeliveryFuture;

use crate::chunk::plan_framing;
use crate::chunk::ChunkList;
use crate::chunk::Framing;
use crate::consts::MAX_CHUNK_ENVELOPE_OVERHEAD;
use crate::consts::TRANSPORT_CUSTOM_OVERHEAD;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::dht::Did;
use crate::dht::LiveDid;
use crate::dht::PeerRing;
use crate::error::Error;
use crate::error::Result;
use crate::measure::MeasureImpl;
use crate::message::ConnectNodeReport;
use crate::message::ConnectNodeSend;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::message::RenegotiateReport;
use crate::message::RenegotiateSend;
use crate::session::SessionSk;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::negotiation::Decision;
use crate::swarm::negotiation::NegotiationEvent;
use crate::swarm::negotiation::Negotiator;

/// The active backend's local media-track type (`NativeMediaTrack` natively, `BrowserMediaTrack` on
/// wasm, `()` under the dummy backend). Named once here so the swarm/node media API can refer to it
/// without committing call sites to a particular backend, and so the outbound media path stays typed
/// (no trait-object downcast).
pub type LocalMediaTrack = <ConnectionOwner as ConnectionInterface>::LocalMediaTrack;

pub struct SwarmTransport {
    pub(crate) network_id: u32,
    transport: Transport,
    session_sk: SessionSk,
    pub(crate) dht: Arc<PeerRing>,
    #[allow(dead_code)]
    measure: Option<MeasureImpl>,
    /// One [`Negotiator`] per peer, guarding renegotiation signaling so only one local offer is ever
    /// outstanding on a connection and stale answers are dropped. See [`crate::swarm::negotiation`].
    negotiations: DashMap<Did, Arc<FuturesMutex<Negotiator>>>,
}

#[derive(Clone)]
pub struct SwarmConnection {
    peer: Did,
    pub connection: ConnectionRef<ConnectionOwner>,
}

/// How a renegotiation effect failed, which decides how the shell reconciles the
/// [`Negotiator`](crate::swarm::negotiation::Negotiator) state with reality.
///
/// The boundary that matters is whether a WebRTC signaling effect (`setLocalDescription` /
/// `setRemoteDescription`) has run yet: before it, the connection is untouched and a failure is just
/// undone; after it, the `PeerConnection` is in a half-negotiated state that cannot be made
/// consistent by reverting pure state, so the connection is reset.
enum RenegotiationError {
    /// Failed during preflight (malformed SDP, connection lookup) — no WebRTC state was mutated, so
    /// the pure decision is rolled back and the connection is left untouched.
    PreEffect(Error),
    /// Failed after a WebRTC signaling effect ran — the connection is reset.
    PostEffect(Error),
}

/// Drive a message's [DeliveryFuture] to completion on the runtime, logging if
/// the message was lost before it could be flushed. This keeps delivery
/// tracking confined to the send site: the status never propagates up through
/// the swarm/node layers.
#[cfg(feature = "wasm")]
fn spawn_delivery(fut: DeliveryFuture, did: Did) {
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = fut.await {
            tracing::warn!("Message to {did} was not delivered: {e}");
        }
    });
}

/// Drive a message's [DeliveryFuture] to completion on the runtime, logging if
/// the message was lost before it could be flushed.
#[cfg(not(feature = "wasm"))]
fn spawn_delivery(fut: DeliveryFuture, did: Did) {
    tokio::spawn(async move {
        if let Err(e) = fut.await {
            tracing::warn!("Message to {did} was not delivered: {e}");
        }
    });
}

impl SwarmTransport {
    pub fn new(
        network_id: u32,
        ice_servers: &str,
        external_address: Option<String>,
        session_sk: SessionSk,
        dht: Arc<PeerRing>,
        measure: Option<MeasureImpl>,
        channel_config: ChannelConfig,
    ) -> Self {
        Self {
            network_id,
            transport: Transport::new(ice_servers, external_address, channel_config),
            session_sk,
            dht,
            measure,
            negotiations: DashMap::new(),
        }
    }

    /// The [`Negotiator`] for `peer`, creating an idle one on first use. Callers lock it for the
    /// duration of one signaling step so renegotiations with a peer are serialized.
    pub(crate) fn negotiator(&self, peer: Did) -> Arc<FuturesMutex<Negotiator>> {
        self.negotiations.entry(peer).or_default().clone()
    }

    /// Forget any renegotiation state for `peer`. Called whenever the peer's connection is removed or
    /// replaced, so a later reconnect to the same did starts from a fresh [`Negotiator`] rather than
    /// inheriting stale `AwaitingAnswer`/generation state from the previous connection.
    pub(crate) fn clear_negotiation(&self, peer: Did) {
        self.negotiations.remove(&peer);
    }

    /// Create new connection that will be handled by swarm.
    pub async fn new_connection(&self, peer: Did, callback: InnerSwarmCallback) -> Result<()> {
        if peer == self.dht.did {
            return Ok(());
        }

        let cid = peer.to_string();
        self.transport
            .new_connection(&cid, Box::new(callback))
            .await
            .map_err(Error::Transport)
    }

    /// Get connection by did.
    pub fn get_connection(&self, peer: Did) -> Option<SwarmConnection> {
        self.transport
            .connection(&peer.to_string())
            .map(|conn| SwarmConnection {
                peer,
                connection: conn,
            })
            .ok()
    }

    /// Get all connections in transport.
    pub fn get_connections(&self) -> Vec<(Did, SwarmConnection)> {
        self.transport
            .connections()
            .into_iter()
            .filter_map(|(k, v)| {
                Did::from_str(&k).ok().map(|did| {
                    (did, SwarmConnection {
                        peer: did,
                        connection: v,
                    })
                })
            })
            .collect()
    }

    /// Get dids of all connections in transport.
    pub fn get_connection_ids(&self) -> Vec<Did> {
        self.transport
            .connection_ids()
            .into_iter()
            .filter_map(|k| Did::from_str(&k).ok())
            .collect()
    }

    /// Disconnect a connection. There are three steps:
    /// 1) remove from DHT;
    /// 2) remove from Transport;
    /// 3) close the connection;
    pub async fn disconnect(&self, peer: Did) -> Result<()> {
        tracing::info!("removing {peer} from DHT");
        self.dht.remove(peer)?;
        // The connection is going away; drop any renegotiation state so a reconnect starts fresh.
        self.clear_negotiation(peer);
        self.transport
            .close_connection(&peer.to_string())
            .await
            .map_err(|e| e.into())
    }

    /// Connect a given Did. If the did is already connected, return Err,
    /// else try prepare offer and establish connection by dht.
    pub async fn connect(&self, peer: Did, callback: InnerSwarmCallback) -> Result<()> {
        let offer_msg = self.prepare_connection_offer(peer, callback).await?;
        self.send_message(Message::ConnectNodeSend(offer_msg), peer)
            .await?;
        Ok(())
    }

    /// Get connection by did and check if data channel is open.
    /// This method will return None if the connection is not found.
    /// This method will wait_for_data_channel_open.
    /// If it's not ready in 8 seconds this method will close it and return None.
    /// If it's ready in 8 seconds this method will return the connection.
    /// See more information about [rings_transport::core::transport::WebrtcConnectionState].
    /// See also method webrtc_wait_for_data_channel_open [rings_transport::core::transport::ConnectionInterface].
    pub async fn get_and_check_connection(&self, peer: Did) -> Option<SwarmConnection> {
        let conn = self.get_connection(peer)?;

        if let Err(e) = conn.connection.webrtc_wait_for_data_channel_open().await {
            tracing::warn!(
                "[get_and_check_connection] connection {peer} data channel not open, will be dropped, reason: {e:?}"
            );

            if let Err(e) = self.disconnect(peer).await {
                tracing::error!("Failed on close connection {peer}: {e:?}");
            }

            return None;
        };

        Some(conn)
    }

    /// Create new connection and its offer.
    pub async fn prepare_connection_offer(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
    ) -> Result<ConnectNodeSend> {
        if self.get_and_check_connection(peer).await.is_some() {
            return Err(Error::AlreadyConnected);
        };

        self.new_connection(peer, callback).await?;
        let conn = self
            .transport
            .connection(&peer.to_string())
            .map_err(Error::Transport)?;

        let offer = conn.webrtc_create_offer().await.map_err(Error::Transport)?;
        let offer_str = serde_json::to_string(&offer).map_err(|_| Error::SerializeToString)?;
        let offer_msg = ConnectNodeSend {
            sdp: offer_str,
            network_id: self.network_id,
        };

        Ok(offer_msg)
    }

    /// Answer the offer of remote connection.
    pub async fn answer_remote_connection(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
        offer_msg: &ConnectNodeSend,
    ) -> Result<ConnectNodeReport> {
        let offer = serde_json::from_str(&offer_msg.sdp).map_err(Error::Deserialize)?;

        if let Some(swarm_conn) = self.get_connection(peer) {
            // Solve the scenario of creating offers simultaneously.
            //
            // When both sides create_offer at the same time and trigger answer_offer of the other side,
            // they will got existed New state connection when answer_offer, which will prevent
            // it to create new connection to answer the offer.
            //
            // The party with a larger Did (ranked lower on the ring) should abandon their own offer and instead answer_offer to the other party.
            // The party with a smaller Did should reject answering the other party and report an Error::AlreadyConnected error.
            if swarm_conn.connection.webrtc_connection_state() == WebrtcConnectionState::New {
                // drop local offer and continue answer remote offer
                if self.dht.did > peer {
                    // this connection will replaced by new connection created bellow
                    self.disconnect(peer).await?;
                } else {
                    // ignore remote offer, and refuse to answer remote offer
                    return Err(Error::AlreadyConnected);
                }
            } else if self.get_and_check_connection(peer).await.is_some() {
                return Err(Error::AlreadyConnected);
            };
        };

        self.new_connection(peer, callback).await?;
        let conn = self
            .transport
            .connection(&peer.to_string())
            .map_err(Error::Transport)?;

        let answer = conn
            .webrtc_answer_offer(offer)
            .await
            .map_err(Error::Transport)?;
        let answer_str = serde_json::to_string(&answer).map_err(|_| Error::SerializeToString)?;
        let answer_msg = ConnectNodeReport { sdp: answer_str };

        Ok(answer_msg)
    }

    /// Accept the answer of remote connection.
    pub async fn accept_remote_connection(
        &self,
        peer: Did,
        answer_msg: &ConnectNodeReport,
    ) -> Result<()> {
        let answer = serde_json::from_str(&answer_msg.sdp).map_err(Error::Deserialize)?;

        let conn = self
            .transport
            .connection(&peer.to_string())
            .map_err(Error::Transport)?;
        conn.webrtc_accept_answer(answer)
            .await
            .map_err(Error::Transport)?;

        Ok(())
    }

    /// Attach a local media track to `peer`'s connection and renegotiate, as one transaction: on any
    /// error the connection is left exactly as it was before the call (no half-attached track).
    ///
    /// The pure [`Negotiator`] decides first: `decide(LocalRenegotiate)` admits the renegotiation or
    /// answers `Busy` ([`Error::RenegotiationInProgress`]) without touching the connection. On the
    /// granted path the track is staged — a clean attach failure (data-only / kind mismatch) mutates
    /// nothing — and then the offer is created and sent. If that fails **before** mutating signaling
    /// state (pre-apply), the staged track is detached so the connection returns to its prior state;
    /// if it fails **after** (`setLocalDescription` ran), the connection is reset (which discards the
    /// track too — see [`reconcile_renegotiation`]). The negotiator is committed only on success, so
    /// pure state and connection state move together.
    ///
    /// [`reconcile_renegotiation`]: Self::reconcile_renegotiation
    pub async fn add_media_track(&self, peer: Did, track: LocalMediaTrack) -> Result<()> {
        let conn = self
            .get_connection(peer)
            .ok_or(Error::SwarmMissDidInTable(peer))?;

        let negotiator = self.negotiator(peer);
        let state = negotiator.lock().await;

        let decision = state.decide(NegotiationEvent::LocalRenegotiate);
        let generation = match decision {
            Decision::SendOffer { generation } => generation,
            // A renegotiation is already in flight; do not attach the track or offer a second time.
            Decision::Busy => return Err(Error::RenegotiationInProgress(peer)),
            // No other decision is reachable for a `LocalRenegotiate` event.
            _ => return Ok(()),
        };

        // Stage the track. A clean attach failure mutates nothing, so the negotiator is left as-is.
        let track_id = match conn.add_media_track(track).await {
            Ok(track_id) => track_id,
            Err(e) => return Err(e),
        };

        let outcome = self.send_local_offer_effect(&conn, peer, generation).await;
        // Keep the whole "attach + renegotiate" a transaction: if the offer failed *before* mutating
        // signaling state (pre-apply), the track was attached but never negotiated, so detach it to
        // return the connection to exactly its prior state — leaving no half-attached track. (A
        // post-apply failure resets the whole connection, which discards the track anyway.)
        if matches!(outcome, Err(RenegotiationError::PreEffect(_))) {
            if let Err(e) = conn.remove_media_track(&track_id).await {
                tracing::warn!("failed to detach staged track {track_id} after a pre-apply offer failure: {e:?}");
            }
        }
        self.reconcile_renegotiation(peer, state, decision, outcome)
            .await
    }

    /// Apply a remote renegotiation offer and, if the [`Negotiator`] accepts, answer it on the live
    /// connection **and send the answer** — one transaction. `ctx` routes the `RenegotiateReport`.
    ///
    /// The pure decision runs first (`decide`, using only the offer's generation and politeness), so
    /// an `Ignore` — the impolite side under glare — returns without parsing the SDP or touching the
    /// connection. Only the `SendAnswer` path parses the offer, then runs the WebRTC effect; the
    /// result is reconciled by [`reconcile_renegotiation`](Self::reconcile_renegotiation).
    pub async fn handle_renegotiation_offer(
        &self,
        peer: Did,
        ctx: &MessagePayload,
        offer_msg: &RenegotiateSend,
    ) -> Result<()> {
        let negotiator = self.negotiator(peer);
        let state = negotiator.lock().await;
        let polite = Negotiator::polite(self.dht.did, peer);
        let decision = state.decide(NegotiationEvent::RemoteOffer {
            generation: offer_msg.generation,
            polite,
        });
        match decision {
            Decision::SendAnswer { .. } => {
                let outcome = self.answer_offer_effect(peer, ctx, offer_msg).await;
                self.reconcile_renegotiation(peer, state, decision, outcome)
                    .await
            }
            // `Ignore`: impolite side under glare keeps its own offer and drops this one — no parse,
            // no lookup, no effect.
            _ => Ok(()),
        }
    }

    /// Apply a remote renegotiation answer. The pure decision runs first: a stale answer (wrong
    /// generation, or none outstanding) is `Ignore`d here without parsing the SDP or touching the
    /// connection. Only the `AcceptAnswer` path parses the answer and runs `setRemoteDescription`;
    /// the result is reconciled by [`reconcile_renegotiation`](Self::reconcile_renegotiation).
    pub async fn handle_renegotiation_answer(
        &self,
        peer: Did,
        answer_msg: &RenegotiateReport,
    ) -> Result<()> {
        let negotiator = self.negotiator(peer);
        let state = negotiator.lock().await;
        let decision = state.decide(NegotiationEvent::RemoteAnswer {
            generation: answer_msg.generation,
        });
        match decision {
            Decision::AcceptAnswer => {
                let outcome = self.accept_answer_effect(peer, answer_msg).await;
                self.reconcile_renegotiation(peer, state, decision, outcome)
                    .await
            }
            // `Ignore`: stale answer — no parse, no lookup.
            _ => Ok(()),
        }
    }

    /// Offer effect for a granted local renegotiation: create the offer and send it point-to-point.
    /// The connection was already located by the caller, so there is no connection-lookup preflight.
    ///
    /// The backend declares the boundary inside `webrtc_create_offer` (`createOffer` is pre-apply;
    /// `setLocalDescription` onward is post-apply), so classify the create-offer failure by what it
    /// reports rather than hardcoding it. A pre-apply create-offer failure leaves the connection's
    /// signaling state untouched, so the caller can detach the staged track and keep the connection;
    /// only `setLocalDescription`/gather/send failures — post-apply — reset the connection.
    async fn send_local_offer_effect(
        &self,
        conn: &SwarmConnection,
        peer: Did,
        generation: u64,
    ) -> std::result::Result<(), RenegotiationError> {
        let offer = conn
            .connection
            .webrtc_create_offer()
            .await
            .map_err(Self::classify_signaling)?;
        let sdp = serde_json::to_string(&offer)
            .map_err(|_| RenegotiationError::PostEffect(Error::SerializeToString))?;
        let offer_msg = RenegotiateSend {
            sdp,
            network_id: self.network_id,
            generation,
        };
        self.send_direct_message(Message::RenegotiateSend(offer_msg), peer)
            .await
            .map(|_| ())
            .map_err(RenegotiationError::PostEffect)
    }

    /// Answer effect for an accepted remote offer: preflight (parse the offer SDP + locate the
    /// connection — [`PreEffect`](RenegotiationError::PreEffect) on failure, nothing mutated), then
    /// the WebRTC effect (`setRemoteDescription` + `createAnswer` + `setLocalDescription`) and the
    /// answer send ([`PostEffect`](RenegotiationError::PostEffect) on failure).
    async fn answer_offer_effect(
        &self,
        peer: Did,
        ctx: &MessagePayload,
        offer_msg: &RenegotiateSend,
    ) -> std::result::Result<(), RenegotiationError> {
        let offer = serde_json::from_str(&offer_msg.sdp)
            .map_err(|e| RenegotiationError::PreEffect(Error::Deserialize(e)))?;
        let conn = self
            .transport
            .connection(&peer.to_string())
            .map_err(|e| RenegotiationError::PreEffect(Error::Transport(e)))?;

        // The backend declares the pre/post-apply boundary (the malformed-SDP rejection happens
        // before `setRemoteDescription`), so classify by what it reports rather than by guessing.
        let answer = conn
            .webrtc_answer_offer(offer)
            .await
            .map_err(Self::classify_signaling)?;
        let sdp = serde_json::to_string(&answer)
            .map_err(|_| RenegotiationError::PostEffect(Error::SerializeToString))?;
        let report = RenegotiateReport {
            sdp,
            generation: offer_msg.generation,
        };
        self.send_report_message(ctx, Message::RenegotiateReport(report))
            .await
            .map_err(RenegotiationError::PostEffect)
    }

    /// Accept effect for a matching remote answer: preflight (parse the answer SDP + locate the
    /// connection — [`PreEffect`](RenegotiationError::PreEffect) on failure), then
    /// `setRemoteDescription(answer)` ([`PostEffect`](RenegotiationError::PostEffect) on failure).
    async fn accept_answer_effect(
        &self,
        peer: Did,
        answer_msg: &RenegotiateReport,
    ) -> std::result::Result<(), RenegotiationError> {
        let answer = serde_json::from_str(&answer_msg.sdp)
            .map_err(|e| RenegotiationError::PreEffect(Error::Deserialize(e)))?;
        let conn = self
            .transport
            .connection(&peer.to_string())
            .map_err(|e| RenegotiationError::PreEffect(Error::Transport(e)))?;

        conn.webrtc_accept_answer(answer)
            .await
            .map_err(Self::classify_signaling)
    }

    /// Classify a failure from an offer/answer/accept backend call into the
    /// [`RenegotiationError`] phase, trusting the backend's own report of whether it mutated
    /// signaling state ([`is_signaling_pre_apply`](rings_transport::error::Error::is_signaling_pre_apply)).
    fn classify_signaling(error: rings_transport::error::Error) -> RenegotiationError {
        if error.is_signaling_pre_apply() {
            RenegotiationError::PreEffect(Error::Transport(error))
        } else {
            RenegotiationError::PostEffect(Error::Transport(error))
        }
    }

    /// Reconcile the negotiator with the outcome of the effect a [`Decision`] authorized, consuming
    /// the lock guard so the reset path can release it first:
    /// - `Ok`: the effect succeeded, so **now** commit the decision (the only state mutation).
    /// - [`PreEffect`](RenegotiationError::PreEffect): the effect failed before any WebRTC state
    ///   changed; nothing was committed, so the negotiator is already correct — just surface the
    ///   error and leave the connection up.
    /// - [`PostEffect`](RenegotiationError::PostEffect): a WebRTC signaling effect ran and the
    ///   round-trip could not complete, so the connection is half-negotiated; reset it (which also
    ///   clears the negotiator), then surface the error.
    async fn reconcile_renegotiation(
        &self,
        peer: Did,
        mut state: futures::lock::MutexGuard<'_, Negotiator>,
        decision: Decision,
        outcome: std::result::Result<(), RenegotiationError>,
    ) -> Result<()> {
        match outcome {
            Ok(()) => {
                state.commit(decision);
                Ok(())
            }
            Err(RenegotiationError::PreEffect(e)) => Err(e),
            Err(RenegotiationError::PostEffect(e)) => {
                drop(state);
                self.reset_failed_renegotiation(peer).await;
                Err(e)
            }
        }
    }

    /// Reset the connection to `peer` after a renegotiation left it in an inconsistent signaling
    /// state. WebRTC offers no reliable mid-handshake rollback in either backend, so we tear the
    /// connection down ([`disconnect`](Self::disconnect), which also clears the negotiator) and let
    /// the peer reconnect cleanly through the DHT. Best-effort: the teardown error is logged.
    async fn reset_failed_renegotiation(&self, peer: Did) {
        if let Err(e) = self.disconnect(peer).await {
            tracing::warn!("failed to reset {peer} after a failed renegotiation: {e:?}");
        }
    }
}

impl SwarmConnection {
    pub async fn send_data(&self, data: Bytes) -> Result<DeliveryFuture> {
        self.connection
            .send_message(TransportMessage::Custom(data.to_vec()))
            .await
            .map_err(|e| e.into())
    }

    pub fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        self.connection.webrtc_connection_state()
    }

    /// The largest single data-channel message this connection can carry — the negotiated
    /// `max_message_size`. Used to size payload chunks so each wrapped chunk stays within the limit.
    pub fn max_message_size(&self) -> usize {
        self.connection.max_message_size()
    }

    /// Attach a local media track to this connection, to be sent to the peer. The connection must
    /// have been created with a media [`ChannelConfig`]. The track is the backend's concrete
    /// [`LocalMediaTrack`] (`NativeMediaTrack` / `BrowserMediaTrack`), so no downcast is involved.
    /// Returns the attached track's id, so the caller can [`remove_media_track`](Self::remove_media_track)
    /// it again to roll back a renegotiation that could not complete.
    pub async fn add_media_track(&self, track: LocalMediaTrack) -> Result<String> {
        self.connection
            .add_media_track(track)
            .await
            .map_err(|e| Error::Media(e.to_string()))
    }

    /// Detach a track previously attached with [`add_media_track`](Self::add_media_track), by id.
    pub async fn remove_media_track(&self, track_id: &str) -> Result<()> {
        self.connection
            .remove_media_track(track_id)
            .await
            .map_err(|e| Error::Media(e.to_string()))
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl PayloadSender for SwarmTransport {
    fn session_sk(&self) -> &SessionSk {
        &self.session_sk
    }

    fn dht(&self) -> Arc<PeerRing> {
        self.dht.clone()
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

        // Each send returns a `DeliveryFuture` resolving to whether the bytes were actually flushed
        // to the wire. We toss it to the runtime rather than awaiting it, so the send itself stays
        // fire-and-forget; a message lost before flush (e.g. the connection died while buffered) is
        // logged there instead of propagating a delivery status up through every layer.
        //
        // The chunk-vs-whole decision is the pure `plan_framing`, derived from this connection's
        // negotiated `max_message_size` (so a channel with a smaller limit is respected); this
        // block is only the effectful shell that carries it out. The reserves account for the bytes
        // each path adds on the wire: `send_data` wraps every send in `TransportMessage::Custom`
        // (`TRANSPORT_CUSTOM_OVERHEAD`), and a chunk is additionally re-wrapped in a `MessagePayload`
        // (`MAX_CHUNK_ENVELOPE_OVERHEAD`). `None` means the peer's limit is too small to carry even
        // one chunk — a real failure we surface rather than send something it would reject.
        let plan = plan_framing(
            data.len(),
            conn.max_message_size(),
            TRANSPORT_CUSTOM_OVERHEAD,
            MAX_CHUNK_ENVELOPE_OVERHEAD + TRANSPORT_CUSTOM_OVERHEAD,
        )
        .ok_or(Error::PeerMaxMessageSizeTooSmall(conn.max_message_size()))?;
        match plan {
            Framing::Whole => {
                spawn_delivery(conn.send_data(data).await?, did);
            }
            Framing::Chunked { chunk_size } => {
                for chunk in ChunkList::split(&data, chunk_size) {
                    let data = MessagePayload::new_send(
                        Message::Chunk(chunk),
                        &self.session_sk,
                        did,
                        did,
                    )?
                    .to_bincode()?;
                    spawn_delivery(conn.send_data(data).await?, did);
                }
            }
        }

        tracing::debug!(
            "Sent {:?}, to node {:?}",
            payload.clone(),
            payload.relay.next_hop,
        );

        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl LiveDid for SwarmConnection {
    async fn live(&self) -> bool {
        self.webrtc_connection_state() == WebrtcConnectionState::Connected
    }
}

impl From<SwarmConnection> for Did {
    fn from(conn: SwarmConnection) -> Self {
        conn.peer
    }
}
