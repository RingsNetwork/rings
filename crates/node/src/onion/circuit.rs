//! Encrypted onion circuit data plane.
//!
//! Security model: forward layers are wrapped from exit to entry with the selected hop session
//! public keys. Each relay decrypts exactly one ElGamal-AEAD layer and learns only the immediate
//! next hop plus an opaque inner layer. Backward frames carry a client-encrypted AEAD payload and
//! relays forward them with local return state.

use std::collections::BTreeMap;

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::ecc::elgamal::impls::secp256k1::encrypt_aead_with_rng;
use rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext;
use rings_core::ecc::PublicKey;
use rings_core::session::SessionSk;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Ctx;
use crate::extension::ext::Interpret;
use crate::extension::ext::Protocol;
use crate::extension::ext::Reject;
use crate::extension::ext::Scope;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteHop;

/// Namespace used by route-aware onion circuit messages.
pub const ONION_CIRCUIT_NAMESPACE: &str = "onion-circuit";

/// Security mode implemented by the current circuit wire format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionCircuitSecurity {
    /// Layered ElGamal-AEAD forward frames with client-encrypted backward payloads.
    LayeredAead,
}

/// Current circuit security mode.
pub const ONION_CIRCUIT_SECURITY: OnionCircuitSecurity = OnionCircuitSecurity::LayeredAead;

/// Maximum number of encrypted hops accepted in one circuit.
pub const MAX_ONION_CIRCUIT_HOPS: u8 = 8;

const MAX_ONION_RELAY_CIRCUITS: usize = 1024;
const ONION_AEAD_NAMESPACE: &str = "rings-node:onion-circuit:v1";

/// One browser HTTPS request executed by an HTTPS exit.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionHttpsRequest {
    /// Target authority (`host:port`).
    pub target: String,
    /// HTTP method.
    pub method: String,
    /// Path and query.
    pub path: String,
    /// Request headers.
    pub headers: Vec<(String, String)>,
    /// Request body bytes.
    pub body: Vec<u8>,
}

/// One browser HTTPS response returned by an HTTPS exit.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionHttpsResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// Payload carried over a route-aware onion circuit.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum OnionCircuitPayload {
    /// Browser-compatible HTTPS request.
    HttpsRequest(OnionHttpsRequest),
    /// Browser-compatible HTTPS response.
    HttpsResponse(OnionHttpsResponse),
    /// Browser-compatible HTTPS error.
    HttpsError(String),
    /// Open a native TCP stream at the exit.
    TcpOpen {
        /// Target authority (`host:port`).
        target: String,
    },
    /// TCP stream data.
    TcpData {
        /// Raw stream bytes.
        bytes: Bytes,
    },
    /// TCP half-close.
    TcpShutdown,
    /// TCP full close.
    TcpClose,
    /// TCP stream error.
    TcpError {
        /// Error message.
        message: String,
    },
}

/// Client return key encrypted into the exit layer.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionClientReturn {
    /// Client session public key used for backward AEAD payloads.
    pub session_public_key: PublicKey<33>,
}

impl OnionClientReturn {
    /// Build a client return descriptor.
    pub const fn new(session_public_key: PublicKey<33>) -> Self {
        Self { session_public_key }
    }
}

/// Public unlinkable circuit correlation id.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OnionCircuitId([u8; 16]);

impl OnionCircuitId {
    /// Build a circuit id from random bytes.
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Generate a random circuit id.
    pub fn random() -> Self {
        Self(rand::random())
    }
}

/// Forward direction: client -> relays -> exit.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionForwardFrame {
    /// Random circuit correlation id.
    pub circuit_id: OnionCircuitId,
    /// AEAD-encrypted layer for the receiving hop.
    pub layer: AeadCiphertext,
}

/// Backward direction: exit -> relays -> client.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionBackwardFrame {
    /// Random circuit correlation id.
    pub circuit_id: OnionCircuitId,
    /// Whether this frame closes relay return state.
    pub terminal: bool,
    /// AEAD payload encrypted to the client session public key.
    pub payload: AeadCiphertext,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
enum OnionForwardLayer {
    Relay {
        next_hop: Did,
        remaining_hops: u8,
        inner: AeadCiphertext,
    },
    Exit {
        client: OnionClientReturn,
        payload: OnionCircuitPayload,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct RelayReturnKey {
    circuit_id: OnionCircuitId,
    next_hop: Did,
}

/// Stateful return-hop table for encrypted relay circuits.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OnionCircuitState {
    relay_returns: BTreeMap<RelayReturnKey, Did>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
enum OnionCircuitMessage {
    Forward(OnionForwardFrame),
    Backward(OnionBackwardFrame),
}

/// One decoded circuit event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionCircuitEvent {
    from: Did,
    message: OnionCircuitMessage,
}

/// Effects emitted by the route-aware circuit reducer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OnionCircuitEffect {
    /// Send a frame to the next hop.
    Send {
        /// Next hop.
        to: Did,
        /// Encoded frame.
        payload: Bytes,
    },
    /// A forward frame reached the exit.
    Exit {
        /// Authenticated immediate sender.
        from: Did,
        /// Random circuit correlation id.
        circuit_id: OnionCircuitId,
        /// Immediate return peer.
        return_peer: Did,
        /// Client return key.
        client: OnionClientReturn,
        /// Application payload.
        payload: OnionCircuitPayload,
    },
    /// A backward frame reached the client.
    Client {
        /// Authenticated immediate sender.
        from: Did,
        /// Random circuit correlation id.
        circuit_id: OnionCircuitId,
        /// Application payload.
        payload: OnionCircuitPayload,
    },
}

/// Encrypted onion circuit protocol.
#[derive(Clone, Debug)]
pub struct OnionCircuitProtocol {
    session_sk: SessionSk,
    allow_relay: bool,
    max_hops: u8,
    max_relay_circuits: usize,
}

impl OnionCircuitProtocol {
    /// Create a protocol instance for the local session.
    pub fn new(session_sk: SessionSk, allow_relay: bool) -> Self {
        Self {
            session_sk,
            allow_relay,
            max_hops: MAX_ONION_CIRCUIT_HOPS,
            max_relay_circuits: MAX_ONION_RELAY_CIRCUITS,
        }
    }
}

impl Protocol for OnionCircuitProtocol {
    type State = OnionCircuitState;
    type Event = OnionCircuitEvent;
    type Effect = OnionCircuitEffect;

    fn namespace(&self) -> &str {
        ONION_CIRCUIT_NAMESPACE
    }

    fn init(&self) -> Self::State {
        OnionCircuitState::default()
    }

    fn decode(&self, wire: Wire<'_>) -> std::result::Result<Self::Event, Reject> {
        let message = bincode::deserialize::<OnionCircuitMessage>(wire.payload)
            .map_err(|error| Reject(format!("bad onion circuit message: {error}")))?;
        Ok(OnionCircuitEvent {
            from: wire.from,
            message,
        })
    }

    fn step(
        &self,
        ctx: Ctx<'_, Self::State>,
        event: Self::Event,
    ) -> Transition<Self::State, Self::Effect> {
        let mut state = ctx.state.clone();
        let effect = match event.message {
            OnionCircuitMessage::Forward(frame) => {
                self.advance_forward(event.from, frame, &mut state)
            }
            OnionCircuitMessage::Backward(frame) => {
                self.advance_backward(ctx.did, event.from, frame, &mut state)
            }
        };

        match effect {
            Ok(effect) => Transition::with(state, vec![effect]),
            Err(error) => {
                tracing::debug!("drop onion circuit message: {error}");
                Transition::pure(state)
            }
        }
    }
}

impl OnionCircuitProtocol {
    fn advance_forward(
        &self,
        from: Did,
        frame: OnionForwardFrame,
        state: &mut OnionCircuitState,
    ) -> Result<OnionCircuitEffect> {
        let layer = decrypt_forward_layer(&self.session_sk, frame.circuit_id, &frame.layer)?;
        match layer {
            OnionForwardLayer::Relay {
                next_hop,
                remaining_hops,
                inner,
            } => {
                self.validate_relay_forward(remaining_hops)?;
                remember_return_hop(
                    state,
                    self.max_relay_circuits,
                    RelayReturnKey {
                        circuit_id: frame.circuit_id,
                        next_hop,
                    },
                    from,
                )?;
                encode_message(OnionCircuitMessage::Forward(OnionForwardFrame {
                    circuit_id: frame.circuit_id,
                    layer: inner,
                }))
                .map(|payload| OnionCircuitEffect::Send {
                    to: next_hop,
                    payload,
                })
            }
            OnionForwardLayer::Exit { client, payload } => Ok(OnionCircuitEffect::Exit {
                from,
                circuit_id: frame.circuit_id,
                return_peer: from,
                client,
                payload,
            }),
        }
    }

    fn advance_backward(
        &self,
        _local: Did,
        from: Did,
        frame: OnionBackwardFrame,
        state: &mut OnionCircuitState,
    ) -> Result<OnionCircuitEffect> {
        let key = RelayReturnKey {
            circuit_id: frame.circuit_id,
            next_hop: from,
        };
        if let Some(previous_hop) = state.relay_returns.get(&key).copied() {
            if frame.terminal {
                state.relay_returns.remove(&key);
            }
            let payload = encode_message(OnionCircuitMessage::Backward(frame))?;
            return Ok(OnionCircuitEffect::Send {
                to: previous_hop,
                payload,
            });
        }

        let payload = decrypt_client_payload(&self.session_sk, frame.circuit_id, &frame.payload)?;
        Ok(OnionCircuitEffect::Client {
            from,
            circuit_id: frame.circuit_id,
            payload,
        })
    }

    fn validate_relay_forward(&self, remaining_hops: u8) -> Result<()> {
        if !self.allow_relay {
            return Err(Error::NoPermission);
        }
        if remaining_hops == 0 || remaining_hops > self.max_hops {
            return Err(Error::OnionRouteError(format!(
                "invalid onion relay hop count {remaining_hops}"
            )));
        }
        Ok(())
    }
}

/// Interpreter for route-aware circuit effects.
pub struct OnionCircuitShell<H> {
    handler: H,
}

impl<H> OnionCircuitShell<H> {
    /// Create a circuit interpreter backed by `handler`.
    pub const fn new(handler: H) -> Self {
        Self { handler }
    }
}

#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
impl<H> Interpret for OnionCircuitShell<H>
where H: OnionCircuitHandler + crate::extension::ext::MaybeSend + 'static
{
    type Effect = OnionCircuitEffect;

    async fn run(&self, scope: &Scope, effect: OnionCircuitEffect) -> Result<Vec<Bytes>> {
        match effect {
            OnionCircuitEffect::Send { to, payload } => {
                scope.send(to, payload).await?;
            }
            OnionCircuitEffect::Exit {
                from,
                circuit_id,
                return_peer,
                client,
                payload,
            } => {
                self.handler
                    .handle_exit(scope, from, circuit_id, return_peer, client, payload)
                    .await?;
            }
            OnionCircuitEffect::Client {
                from,
                circuit_id,
                payload,
            } => {
                self.handler
                    .handle_client(scope, from, circuit_id, payload)
                    .await?;
            }
        }
        Ok(Vec::new())
    }
}

/// Runtime-specific circuit handling.
#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
pub trait OnionCircuitHandler {
    /// Handle a frame that reached this node as the exit.
    async fn handle_exit(
        &self,
        scope: &Scope,
        from: Did,
        circuit_id: OnionCircuitId,
        return_peer: Did,
        client: OnionClientReturn,
        payload: OnionCircuitPayload,
    ) -> Result<()>;

    /// Handle a frame that reached this node as the client.
    async fn handle_client(
        &self,
        scope: &Scope,
        from: Did,
        circuit_id: OnionCircuitId,
        payload: OnionCircuitPayload,
    ) -> Result<()>;
}

/// Encode the first forward frame for `route`.
pub fn encode_initial_forward(
    client: OnionClientReturn,
    route: &OnionRoute,
    circuit_id: OnionCircuitId,
    payload: OnionCircuitPayload,
) -> Result<(Did, Bytes)> {
    let Some(first) = route.encryption_hops.first().copied() else {
        return Err(Error::OnionRouteError(
            "onion route has no hops".to_string(),
        ));
    };
    validate_route_hop_count(route.encryption_hops.len())?;
    let layer = build_forward_layers(client, &route.encryption_hops, circuit_id, payload)?;
    let frame = OnionForwardFrame { circuit_id, layer };
    encode_message(OnionCircuitMessage::Forward(frame)).map(|payload| (first.did, payload))
}

/// Send a response payload back to the immediate return peer.
pub async fn send_backward(
    scope: &Scope,
    circuit_id: OnionCircuitId,
    return_peer: Did,
    client: OnionClientReturn,
    payload: OnionCircuitPayload,
) -> Result<()> {
    let frame = OnionBackwardFrame {
        circuit_id,
        terminal: payload_closes_circuit(&payload),
        payload: encrypt_client_payload(circuit_id, payload, client.session_public_key)?,
    };
    let payload = encode_message(OnionCircuitMessage::Backward(frame))?;
    scope.send(return_peer, payload).await
}

fn build_forward_layers(
    client: OnionClientReturn,
    hops: &[OnionRouteHop],
    circuit_id: OnionCircuitId,
    payload: OnionCircuitPayload,
) -> Result<AeadCiphertext> {
    let Some(exit) = hops.last().copied() else {
        return Err(Error::OnionRouteError(
            "onion route has no hops".to_string(),
        ));
    };
    let mut layer = encrypt_forward_layer(
        circuit_id,
        OnionForwardLayer::Exit { client, payload },
        exit.session_public_key,
    )?;

    for (index, hop) in hops.iter().copied().enumerate().rev().skip(1) {
        let next_hop = hops
            .get(index.saturating_add(1))
            .map(|next| next.did)
            .ok_or_else(|| Error::OnionRouteError("missing next onion hop".to_string()))?;
        let remaining_hops = u8::try_from(hops.len().saturating_sub(index + 1))
            .map_err(|_| Error::OnionRouteError("onion route is too long".to_string()))?;
        layer = encrypt_forward_layer(
            circuit_id,
            OnionForwardLayer::Relay {
                next_hop,
                remaining_hops,
                inner: layer,
            },
            hop.session_public_key,
        )?;
    }
    Ok(layer)
}

fn encrypt_forward_layer(
    circuit_id: OnionCircuitId,
    layer: OnionForwardLayer,
    recipient: PublicKey<33>,
) -> Result<AeadCiphertext> {
    let plaintext = bincode::serialize(&layer).map_err(|_| Error::EncodeError)?;
    let aad = onion_aead_context(OnionAeadDirection::Forward, circuit_id)?;
    let mut rng = rand::thread_rng();
    encrypt_aead_with_rng(&plaintext, &aad, recipient, &mut rng).map_err(Error::CoreError)
}

fn decrypt_forward_layer(
    session_sk: &SessionSk,
    circuit_id: OnionCircuitId,
    sealed: &AeadCiphertext,
) -> Result<OnionForwardLayer> {
    let aad = onion_aead_context(OnionAeadDirection::Forward, circuit_id)?;
    let plaintext = session_sk
        .decrypt_elgamal_aead(sealed, &aad)
        .map_err(Error::CoreError)?;
    bincode::deserialize(&plaintext).map_err(|_| Error::DecodeError)
}

fn encrypt_client_payload(
    circuit_id: OnionCircuitId,
    payload: OnionCircuitPayload,
    recipient: PublicKey<33>,
) -> Result<AeadCiphertext> {
    let plaintext = bincode::serialize(&payload).map_err(|_| Error::EncodeError)?;
    let aad = onion_aead_context(OnionAeadDirection::Backward, circuit_id)?;
    let mut rng = rand::thread_rng();
    encrypt_aead_with_rng(&plaintext, &aad, recipient, &mut rng).map_err(Error::CoreError)
}

fn decrypt_client_payload(
    session_sk: &SessionSk,
    circuit_id: OnionCircuitId,
    sealed: &AeadCiphertext,
) -> Result<OnionCircuitPayload> {
    let aad = onion_aead_context(OnionAeadDirection::Backward, circuit_id)?;
    let plaintext = session_sk
        .decrypt_elgamal_aead(sealed, &aad)
        .map_err(Error::CoreError)?;
    bincode::deserialize(&plaintext).map_err(|_| Error::DecodeError)
}

#[derive(Serialize)]
struct OnionAeadContext {
    namespace: &'static str,
    direction: OnionAeadDirection,
    circuit_id: OnionCircuitId,
}

#[derive(Clone, Copy, Serialize)]
enum OnionAeadDirection {
    Forward,
    Backward,
}

fn onion_aead_context(
    direction: OnionAeadDirection,
    circuit_id: OnionCircuitId,
) -> Result<Vec<u8>> {
    bincode::serialize(&OnionAeadContext {
        namespace: ONION_AEAD_NAMESPACE,
        direction,
        circuit_id,
    })
    .map_err(|_| Error::EncodeError)
}

fn validate_route_hop_count(hops: usize) -> Result<()> {
    if hops == 0 || hops > MAX_ONION_CIRCUIT_HOPS as usize {
        return Err(Error::OnionRouteError(format!(
            "onion route hop count {hops} exceeds limit {MAX_ONION_CIRCUIT_HOPS}"
        )));
    }
    Ok(())
}

fn remember_return_hop(
    state: &mut OnionCircuitState,
    max_relay_circuits: usize,
    key: RelayReturnKey,
    previous_hop: Did,
) -> Result<()> {
    if !state.relay_returns.contains_key(&key) && state.relay_returns.len() >= max_relay_circuits {
        return Err(Error::OnionRouteError(
            "onion relay circuit table is full".to_string(),
        ));
    }
    state.relay_returns.insert(key, previous_hop);
    Ok(())
}

fn payload_closes_circuit(payload: &OnionCircuitPayload) -> bool {
    matches!(
        payload,
        OnionCircuitPayload::HttpsResponse(_)
            | OnionCircuitPayload::HttpsError(_)
            | OnionCircuitPayload::TcpClose
            | OnionCircuitPayload::TcpError { .. }
    )
}

fn encode_message(message: OnionCircuitMessage) -> Result<Bytes> {
    bincode::serialize(&message)
        .map(Bytes::from)
        .map_err(|_| Error::EncodeError)
}

#[cfg(test)]
mod tests {
    use rings_core::ecc::SecretKey;
    use rings_core::session::SessionSk;

    use super::*;
    use crate::onion::OnionExitDescriptor;
    use crate::onion::OnionExitDescriptorBody;
    use crate::onion::OnionExitService;
    use crate::onion::OnionExitTransport;
    use crate::onion::OnionRoute;
    use crate::online::OnlineNodeType;

    fn session() -> SessionSk {
        SessionSk::new_with_seckey(&SecretKey::random()).expect("session key")
    }

    fn route(relays: &[SessionSk], exit_session: &SessionSk) -> OnionRoute {
        let exit = exit_session.account_did();
        let public_key = exit_session
            .session()
            .account_verification_pubkey()
            .expect("verification key");
        let mut encryption_hops = relays
            .iter()
            .map(|relay| OnionRouteHop::new(relay.account_did(), relay.session_public_key()))
            .collect::<Vec<_>>();
        encryption_hops.push(OnionRouteHop::new(exit, exit_session.session_public_key()));
        let hops = encryption_hops
            .iter()
            .map(|hop| hop.did)
            .collect::<Vec<_>>();
        OnionRoute {
            service: "https".to_string(),
            hops,
            encryption_hops,
            exit: OnionExitDescriptor::new_signed(
                OnionExitDescriptorBody {
                    did: exit,
                    public_key,
                    session_public_key: exit_session.session_public_key(),
                    node_type: OnlineNodeType::Native,
                    network_id: 1,
                    services: vec![OnionExitService {
                        name: "https".to_string(),
                        transport: OnionExitTransport::Https,
                    }],
                    policy: Default::default(),
                    started_at_ms: 0,
                    heartbeat_at_ms: 0,
                    expires_at_ms: 1,
                    version: "test".to_string(),
                },
                exit_session,
            )
            .expect("signed exit"),
        }
    }

    #[test]
    fn initial_forward_targets_first_hop_and_hides_payload() {
        let client = session();
        let first = session();
        let second = session();
        let exit = session();
        let route = route(&[first.clone(), second], &exit);
        let circuit_id = OnionCircuitId::new([9; 16]);

        let (to, payload) = encode_initial_forward(
            OnionClientReturn::new(client.session_public_key()),
            &route,
            circuit_id,
            OnionCircuitPayload::HttpsError("probe".to_string()),
        )
        .expect("encode initial route");
        let decoded =
            bincode::deserialize::<OnionCircuitMessage>(&payload).expect("decode initial route");

        assert_eq!(to, first.account_did());
        let OnionCircuitMessage::Forward(frame) = decoded else {
            panic!("expected forward frame");
        };
        assert_eq!(frame.circuit_id, circuit_id);
        assert!(!format!("{frame:?}").contains(&format!("{:?}", client.account_did())));
        assert!(!format!("{:?}", frame.layer).contains("probe"));
    }

    #[test]
    fn relay_forward_requires_opt_in() {
        let client = session();
        let relay = session();
        let exit = session();
        let route = route(std::slice::from_ref(&relay), &exit);
        let circuit_id = OnionCircuitId::new([1; 16]);
        let (_, payload) = encode_initial_forward(
            OnionClientReturn::new(client.session_public_key()),
            &route,
            circuit_id,
            OnionCircuitPayload::TcpShutdown,
        )
        .expect("encode forward");
        let message = bincode::deserialize::<OnionCircuitMessage>(&payload).expect("decode");
        let protocol = OnionCircuitProtocol::new(relay.clone(), false);
        let state = protocol.init();

        let transition = protocol.step(
            Ctx {
                did: relay.account_did(),
                state: &state,
            },
            OnionCircuitEvent {
                from: client.account_did(),
                message,
            },
        );

        assert!(transition.effects.is_empty());
    }

    #[test]
    fn relay_decrypts_one_layer_and_remembers_return_hop() {
        let client = session();
        let relay = session();
        let exit = session();
        let route = route(std::slice::from_ref(&relay), &exit);
        let circuit_id = OnionCircuitId::new([2; 16]);
        let (_, payload) = encode_initial_forward(
            OnionClientReturn::new(client.session_public_key()),
            &route,
            circuit_id,
            OnionCircuitPayload::TcpShutdown,
        )
        .expect("encode forward");
        let message = bincode::deserialize::<OnionCircuitMessage>(&payload).expect("decode");
        let protocol = OnionCircuitProtocol::new(relay.clone(), true);
        let state = protocol.init();

        let transition = protocol.step(
            Ctx {
                did: relay.account_did(),
                state: &state,
            },
            OnionCircuitEvent {
                from: client.account_did(),
                message,
            },
        );

        assert_eq!(transition.effects.len(), 1);
        assert!(matches!(
            transition.effects.first(),
            Some(OnionCircuitEffect::Send { to, .. }) if *to == exit.account_did()
        ));
        assert_eq!(transition.state.relay_returns.len(), 1);
    }

    #[test]
    fn client_decrypts_backward_payload() {
        let client = session();
        let exit = session();
        let protocol = OnionCircuitProtocol::new(client.clone(), false);
        let state = protocol.init();
        let circuit_id = OnionCircuitId::new([3; 16]);
        let frame = OnionBackwardFrame {
            circuit_id,
            terminal: true,
            payload: encrypt_client_payload(
                circuit_id,
                OnionCircuitPayload::TcpError {
                    message: "closed".to_string(),
                },
                client.session_public_key(),
            )
            .expect("encrypt backward"),
        };

        let transition = protocol.step(
            Ctx {
                did: client.account_did(),
                state: &state,
            },
            OnionCircuitEvent {
                from: exit.account_did(),
                message: OnionCircuitMessage::Backward(frame),
            },
        );

        assert!(matches!(
            transition.effects.first(),
            Some(OnionCircuitEffect::Client {
                from,
                circuit_id: returned_circuit_id,
                payload: OnionCircuitPayload::TcpError { message },
            }) if *from == exit.account_did()
                && *returned_circuit_id == circuit_id
                && message == "closed"
        ));
    }
}
