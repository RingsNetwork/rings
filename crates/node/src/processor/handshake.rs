//! Handshakes of the processor: a connect by DID over the overlay, and a handshake over a peer's
//! public HTTP API.

use std::str::FromStr;

use rings_core::dht::Did;
use rings_core::error::Error as CoreError;
use rings_core::message::Decoder;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::Message;
use rings_core::message::MessagePayload;
use rings_core::swarm::ConnectionAttempt;
use rings_rpc::protos::rings_node::AcceptAnswerRequest;
use rings_rpc::protos::rings_node::AnswerOfferRequest;
use rings_rpc::protos::rings_node::NodeDidRequest;

use super::Processor;
use crate::error::Error;
use crate::error::Result;
use crate::remote_endpoint::remote_rpc_client;
use crate::remote_endpoint::RemoteRpcEndpoint;
/// Which node a handshake over HTTP must end up connected to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HandshakePeer {
    /// Whatever DID the endpoint answers as.
    Any,
    /// Only this DID; any other answer is refused before an offer is created.
    Pinned(Did),
}

/// The remote half of a handshake over HTTP: the peer's public `answerOffer`, and its
/// `acceptAnswer` for a counter-offer this node answered.
#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
pub(crate) trait RemoteHandshake {
    /// The peer's reply to `offer`: an answer, or its own prevailing offer (#947).
    async fn answer_offer(&self, offer: &MessagePayload) -> Result<MessagePayload>;

    /// Deliver `answer` to the peer's own offer.
    async fn accept_answer(&self, answer: &MessagePayload) -> Result<()>;
}

#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
impl RemoteHandshake for rings_rpc::jsonrpc::Client {
    async fn answer_offer(&self, offer: &MessagePayload) -> Result<MessagePayload> {
        let offer = offer.encode().map_err(|_| Error::EncodeError)?;
        let reply = rings_rpc::jsonrpc::Client::answer_offer(self, &AnswerOfferRequest {
            offer: offer.to_string(),
        })
        .await
        .map_err(Error::from)?
        .answer;
        MessagePayload::from_encoded(&Encoded::from(reply)).map_err(|_| Error::DecodeError)
    }

    async fn accept_answer(&self, answer: &MessagePayload) -> Result<()> {
        let answer = answer.encode().map_err(|_| Error::EncodeError)?;
        rings_rpc::jsonrpc::Client::accept_answer(self, &AcceptAnswerRequest {
            answer: answer.to_string(),
        })
        .await
        .map_err(Error::from)?;
        Ok(())
    }
}

/// How an HTTP handshake exchange ended without failing.
enum HttpExchange {
    /// The peer answered and the answer was accepted for this node's generation.
    Accepted,
    /// The peer's own offer prevailed and was answered: the edge is the peer's handshake.
    PeerHandshake,
}

/// Why a handshake over HTTP did not reach an accepted answer.
#[derive(Debug)]
pub(crate) enum HandshakeFailure {
    /// The peer's slot is owned by another attempt (a handshake in flight, whichever side
    /// started it): refused before any request when the local core already holds an unadmitted
    /// handshake, or by the local core during the exchange. Nothing is wrong; a later attempt may
    /// succeed.
    InFlight {
        /// The peer whose slot is owned.
        peer: Did,
    },
    /// The handshake failed.
    Failed(Error),
}

impl HandshakeFailure {
    /// Whether `error` is a core refusal because another attempt owns the peer's slot: an offer
    /// refused because the peer already holds a record, or an attempt superseded by the peer's
    /// own offer (#947). `is_concurrent(e) ⟺ from_stage(p, s, e) = InFlight { p }`.
    pub(crate) const fn is_concurrent(error: &CoreError) -> bool {
        matches!(
            error,
            CoreError::AlreadyConnected | CoreError::ConnectionAttemptSuperseded { .. }
        )
    }

    /// Sort a core refusal of one handshake stage: a concurrent refusal
    /// ([`Self::is_concurrent`]) is `InFlight`; any other error is `Failed` under the stage's
    /// wrapper.
    fn from_stage(peer: Did, stage: fn(CoreError) -> Error, error: CoreError) -> Self {
        if Self::is_concurrent(&error) {
            Self::InFlight { peer }
        } else {
            Self::Failed(stage(error))
        }
    }
}

impl From<Error> for HandshakeFailure {
    /// Every error that is not a core refusal of a handshake stage is a failure.
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

impl From<HandshakeFailure> for Error {
    /// A refusal is reported as `HandshakeInFlight`; a failure as itself.
    fn from(failure: HandshakeFailure) -> Self {
        match failure {
            HandshakeFailure::InFlight { peer } => Error::HandshakeInFlight { peer },
            HandshakeFailure::Failed(error) => error,
        }
    }
}

impl Processor {
    /// Connect peer with web3 did.
    /// There are 3 peers: PeerA, PeerB, PeerC.
    /// 1. PeerA has a connection with PeerB.
    /// 2. PeerC has a connection with PeerB.
    /// 3. PeerC can connect PeerA with PeerA's web3 address.
    ///
    /// This operation is idempotent: if topology convergence already produced
    /// the direct connection, the requested connection is satisfied.
    pub async fn connect_with_did(&self, did: Did) -> Result<()> {
        match self.swarm.connect(did).await {
            Ok(()) | Err(rings_core::error::Error::AlreadyConnected) => Ok(()),
            Err(error) => Err(Error::ConnectError(error)),
        }
    }

    /// Handshake with the node behind `endpoint` over its public HTTP API: learn its DID, and
    /// when `peer` is pinned refuse before any request if the local core already holds an
    /// unadmitted handshake to it, and before any offer if the endpoint answers as another
    /// node; then create, exchange and accept the offer. Returns the generation the offer
    /// reserved once the answer is accepted; admission of the resulting transport completes
    /// asynchronously and reaches the [`Backend`](crate::extension::Backend) as an item of the
    /// core's `ApplicationInbox`. The answer is pinned to that
    /// generation, so it is never applied to a handshake the peer started meanwhile; a failure
    /// after the offer was created cancels the generation iff it is still pending.
    ///
    /// When the peer holds its own sent overlay offer to this node and that offer prevails on
    /// the DID tie-break (#947), the peer replies with that offer instead of an answer. This
    /// node then answers it, which releases its own losing generation, and delivers the answer
    /// with the peer's `acceptAnswer`; the edge is the peer's handshake, still in flight, so
    /// the outcome is `InFlight`, and nothing is abandoned.
    pub(crate) async fn connect_peer_via_http(
        &self,
        endpoint: &RemoteRpcEndpoint,
        api_token: Option<&str>,
        peer: HandshakePeer,
    ) -> std::result::Result<ConnectionAttempt, HandshakeFailure> {
        if let HandshakePeer::Pinned(pinned) = peer {
            if self
                .swarm
                .has_unadmitted_connection(pinned)
                .map_err(Error::ConnectError)?
            {
                return Err(HandshakeFailure::InFlight { peer: pinned });
            }
        }
        let learned = async {
            let client = remote_rpc_client(endpoint, api_token).await?;
            let answered = client.node_did(&NodeDidRequest {}).await?.did;
            let actual =
                Did::from_str(answered.as_str()).map_err(|_| Error::InvalidDid(answered))?;
            if let HandshakePeer::Pinned(expected) = peer {
                if actual != expected {
                    tracing::error!(
                        %endpoint,
                        %expected,
                        %actual,
                        "endpoint answered as another node; handshake refused"
                    );
                    return Err(Error::HandshakePeerMismatch { expected, actual });
                }
            }
            Ok((client, actual))
        };
        let (client, actual) = learned.await?;
        self.exchange_offer(&client, actual).await
    }

    /// Create an offer for `peer`, exchange it with `remote`, and complete the handshake; see
    /// [`Self::connect_peer_via_http`].
    pub(crate) async fn exchange_offer(
        &self,
        remote: &impl RemoteHandshake,
        peer: Did,
    ) -> std::result::Result<ConnectionAttempt, HandshakeFailure> {
        let (attempt, offer) = self
            .swarm
            .offer_connection(peer)
            .await
            .map_err(|error| HandshakeFailure::from_stage(peer, Error::CreateOffer, error))?;
        let exchange = async {
            let reply = remote.answer_offer(&offer).await?;
            if let Message::ConnectNodeSend(_) = reply
                .transaction
                .data::<Message>()
                .map_err(|_| Error::DecodeError)?
            {
                self.answer_counter_offer(remote, peer, reply).await?;
                return Ok(HttpExchange::PeerHandshake);
            }
            self.swarm
                .accept_answer_for(attempt, reply)
                .await
                .map_err(|error| HandshakeFailure::from_stage(peer, Error::AcceptAnswer, error))
                .map(|()| HttpExchange::Accepted)
        };
        match exchange.await {
            Ok(HttpExchange::Accepted) => Ok(attempt),
            Ok(HttpExchange::PeerHandshake) => Err(HandshakeFailure::InFlight { peer }),
            Err(failure) => {
                self.abandon_handshake(attempt).await;
                Err(failure)
            }
        }
    }

    /// Answer the peer's prevailing `counter_offer` (#947) and deliver the answer with the
    /// peer's `acceptAnswer`. Answering releases this node's own losing generation.
    async fn answer_counter_offer(
        &self,
        remote: &impl RemoteHandshake,
        peer: Did,
        counter_offer: MessagePayload,
    ) -> std::result::Result<(), HandshakeFailure> {
        let answer = self
            .swarm
            .answer_offer(counter_offer)
            .await
            .map_err(|error| HandshakeFailure::from_stage(peer, Error::AnswerOffer, error))?
            .answer()
            .ok_or(HandshakeFailure::InFlight { peer })?;
        remote.accept_answer(&answer).await?;
        Ok(())
    }

    /// Cancel the generation `attempt` reserved iff it is still pending, that is, no data
    /// channel has opened on it; one admitting, admitted or superseded meanwhile is left alone,
    /// and a failure to cancel is logged since the core expires an unadmitted attempt on its
    /// own.
    pub(crate) async fn abandon_handshake(&self, attempt: ConnectionAttempt) {
        if let Err(error) = self.swarm.cancel_pending_connection_attempt(attempt).await {
            tracing::warn!(?attempt, %error, "failed to cancel the abandoned handshake");
        }
    }

    /// Whether a seed document owes `peer` a dial: it is not this node and its admission was
    /// not announced. A peer with a handshake in flight is dialed and refused by the core,
    /// which `connect seed` reads as the peer being taken care of.
    pub(crate) fn owes_seed_dial(&self, peer: Did) -> Result<bool> {
        Ok(peer != self.swarm.did() && !self.swarm.is_peer_admitted(peer)?)
    }
}
