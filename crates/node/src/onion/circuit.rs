//! Route-aware onion circuit data plane.

use bytes::Bytes;
use rings_core::dht::Did;
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

/// Namespace used by route-aware onion circuit messages.
pub const ONION_CIRCUIT_NAMESPACE: &str = "onion-circuit";

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

/// Forward direction: client -> relays -> exit.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionForwardFrame {
    /// Client-selected stream id, scoped by the client DID.
    pub stream_id: u64,
    /// Hops that still need to receive this frame before the exit is reached.
    pub remaining_hops: Vec<Did>,
    /// Reverse path, immediate previous hop first.
    pub return_path: Vec<Did>,
    /// Application payload.
    pub payload: OnionCircuitPayload,
}

/// Backward direction: exit -> relays -> client.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionBackwardFrame {
    /// Client-selected stream id, scoped by the client DID.
    pub stream_id: u64,
    /// Return path, next immediate hop first.
    pub return_path: Vec<Did>,
    /// Application payload.
    pub payload: OnionCircuitPayload,
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
        /// Client-selected stream id.
        stream_id: u64,
        /// Reverse path to the client.
        return_path: Vec<Did>,
        /// Application payload.
        payload: OnionCircuitPayload,
    },
    /// A backward frame reached the client.
    Client {
        /// Authenticated immediate sender.
        from: Did,
        /// Client-selected stream id.
        stream_id: u64,
        /// Application payload.
        payload: OnionCircuitPayload,
    },
}

/// Route-aware onion circuit protocol.
#[derive(Clone, Copy, Debug, Default)]
pub struct OnionCircuitProtocol;

impl Protocol for OnionCircuitProtocol {
    type State = ();
    type Event = OnionCircuitEvent;
    type Effect = OnionCircuitEffect;

    fn namespace(&self) -> &str {
        ONION_CIRCUIT_NAMESPACE
    }

    fn init(&self) -> Self::State {}

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
        let effect = match event.message {
            OnionCircuitMessage::Forward(mut frame) => match advance_forward(ctx.did, &mut frame) {
                Some(to) => encode_message(OnionCircuitMessage::Forward(frame))
                    .map(|payload| OnionCircuitEffect::Send { to, payload }),
                None => Ok(OnionCircuitEffect::Exit {
                    from: event.from,
                    stream_id: frame.stream_id,
                    return_path: frame.return_path,
                    payload: frame.payload,
                }),
            },
            OnionCircuitMessage::Backward(mut frame) => match advance_backward(&mut frame) {
                Some(to) => encode_message(OnionCircuitMessage::Backward(frame))
                    .map(|payload| OnionCircuitEffect::Send { to, payload }),
                None => Ok(OnionCircuitEffect::Client {
                    from: event.from,
                    stream_id: frame.stream_id,
                    payload: frame.payload,
                }),
            },
        };

        match effect {
            Ok(effect) => Transition::with((), vec![effect]),
            Err(error) => {
                tracing::debug!("drop onion circuit message: {error}");
                Transition::pure(())
            }
        }
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
                stream_id,
                return_path,
                payload,
            } => {
                self.handler
                    .handle_exit(scope, from, stream_id, return_path, payload)
                    .await?;
            }
            OnionCircuitEffect::Client {
                from,
                stream_id,
                payload,
            } => {
                self.handler
                    .handle_client(scope, from, stream_id, payload)
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
        stream_id: u64,
        return_path: Vec<Did>,
        payload: OnionCircuitPayload,
    ) -> Result<()>;

    /// Handle a frame that reached this node as the client.
    async fn handle_client(
        &self,
        scope: &Scope,
        from: Did,
        stream_id: u64,
        payload: OnionCircuitPayload,
    ) -> Result<()>;
}

/// Encode the first forward frame for `route`.
pub fn encode_initial_forward(
    local: Did,
    route: &OnionRoute,
    stream_id: u64,
    payload: OnionCircuitPayload,
) -> Result<(Did, Bytes)> {
    let Some((first, rest)) = route.hops.split_first() else {
        return Err(Error::OnionRouteError(
            "onion route has no hops".to_string(),
        ));
    };
    let frame = OnionForwardFrame {
        stream_id,
        remaining_hops: rest.to_vec(),
        return_path: vec![local],
        payload,
    };
    encode_message(OnionCircuitMessage::Forward(frame)).map(|payload| (*first, payload))
}

/// Send a response payload back along `return_path`.
pub async fn send_backward(
    scope: &Scope,
    stream_id: u64,
    return_path: Vec<Did>,
    payload: OnionCircuitPayload,
) -> Result<()> {
    let mut frame = OnionBackwardFrame {
        stream_id,
        return_path,
        payload,
    };
    let Some(to) = advance_backward(&mut frame) else {
        return Err(Error::OnionRouteError(
            "onion response has empty return path".to_string(),
        ));
    };
    let payload = encode_message(OnionCircuitMessage::Backward(frame))?;
    scope.send(to, payload).await
}

fn advance_forward(local: Did, frame: &mut OnionForwardFrame) -> Option<Did> {
    if frame.remaining_hops.is_empty() {
        return None;
    }
    let next = frame.remaining_hops.remove(0);
    frame.return_path.insert(0, local);
    Some(next)
}

fn advance_backward(frame: &mut OnionBackwardFrame) -> Option<Did> {
    if frame.return_path.is_empty() {
        return None;
    }
    Some(frame.return_path.remove(0))
}

fn encode_message(message: OnionCircuitMessage) -> Result<Bytes> {
    bincode::serialize(&message)
        .map(Bytes::from)
        .map_err(|_| Error::EncodeError)
}

/// Browser handler for HTTPS onion circuits.
#[cfg(feature = "browser")]
pub(crate) struct BrowserOnionCircuitHandler {
    https: std::sync::Arc<crate::onion_https::OnionHttpsRuntime>,
}

#[cfg(feature = "browser")]
impl BrowserOnionCircuitHandler {
    /// Create a browser circuit handler backed by the HTTPS runtime.
    pub(crate) fn new(https: std::sync::Arc<crate::onion_https::OnionHttpsRuntime>) -> Self {
        Self { https }
    }
}

#[cfg(feature = "browser")]
#[async_trait::async_trait(?Send)]
impl OnionCircuitHandler for BrowserOnionCircuitHandler {
    async fn handle_exit(
        &self,
        scope: &Scope,
        _from: Did,
        stream_id: u64,
        return_path: Vec<Did>,
        payload: OnionCircuitPayload,
    ) -> Result<()> {
        let response = match payload {
            OnionCircuitPayload::HttpsRequest(request) => {
                match crate::onion_https::execute_exit_fetch(&self.https, &request).await {
                    Ok(response) => OnionCircuitPayload::HttpsResponse(response),
                    Err(error) => OnionCircuitPayload::HttpsError(error.to_string()),
                }
            }
            OnionCircuitPayload::TcpOpen { .. }
            | OnionCircuitPayload::TcpData { .. }
            | OnionCircuitPayload::TcpShutdown
            | OnionCircuitPayload::TcpClose
            | OnionCircuitPayload::TcpError { .. } => OnionCircuitPayload::TcpError {
                message: "browser onion exits do not support TCP".to_string(),
            },
            OnionCircuitPayload::HttpsResponse(_) | OnionCircuitPayload::HttpsError(_) => {
                return Ok(());
            }
        };
        send_backward(scope, stream_id, return_path, response).await
    }

    async fn handle_client(
        &self,
        _scope: &Scope,
        from: Did,
        stream_id: u64,
        payload: OnionCircuitPayload,
    ) -> Result<()> {
        match payload {
            OnionCircuitPayload::HttpsResponse(response) => {
                self.https.complete_response(from, stream_id, response);
            }
            OnionCircuitPayload::HttpsError(message) => {
                self.https.complete_error(from, stream_id, message);
            }
            _ => {}
        }
        Ok(())
    }
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

    fn did() -> Did {
        SecretKey::random().address().into()
    }

    fn route(mut relays: Vec<Did>) -> OnionRoute {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key).expect("session key");
        let exit = session_sk.account_did();
        let public_key = session_sk
            .session()
            .account_verification_pubkey()
            .expect("verification key");
        relays.push(exit);
        OnionRoute {
            service: "https".to_string(),
            hops: relays,
            exit: OnionExitDescriptor::new_signed(
                OnionExitDescriptorBody {
                    did: exit,
                    public_key,
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
                &session_sk,
            )
            .expect("signed exit"),
        }
    }

    #[test]
    fn initial_forward_targets_first_hop_and_keeps_rest() {
        let local = did();
        let first = did();
        let second = did();
        let route = route(vec![first, second]);
        let exit = *route.hops.last().expect("route exit");

        let (to, payload) = encode_initial_forward(
            local,
            &route,
            9,
            OnionCircuitPayload::HttpsError("probe".to_string()),
        )
        .expect("encode initial route");
        let decoded =
            bincode::deserialize::<OnionCircuitMessage>(&payload).expect("decode initial route");

        assert_eq!(to, first);
        assert_eq!(
            decoded,
            OnionCircuitMessage::Forward(OnionForwardFrame {
                stream_id: 9,
                remaining_hops: vec![second, exit],
                return_path: vec![local],
                payload: OnionCircuitPayload::HttpsError("probe".to_string()),
            })
        );
    }

    #[test]
    fn relay_forward_prepends_reverse_hop() {
        let client = did();
        let relay = did();
        let exit = did();
        let mut frame = OnionForwardFrame {
            stream_id: 1,
            remaining_hops: vec![exit],
            return_path: vec![client],
            payload: OnionCircuitPayload::TcpShutdown,
        };

        assert_eq!(advance_forward(relay, &mut frame), Some(exit));
        assert_eq!(frame.return_path, vec![relay, client]);
        assert!(frame.remaining_hops.is_empty());
    }

    #[test]
    fn backward_frame_consumes_return_path_one_hop_at_a_time() {
        let relay = did();
        let client = did();
        let mut frame = OnionBackwardFrame {
            stream_id: 2,
            return_path: vec![relay, client],
            payload: OnionCircuitPayload::TcpClose,
        };

        assert_eq!(advance_backward(&mut frame), Some(relay));
        assert_eq!(frame.return_path, vec![client]);
        assert_eq!(advance_backward(&mut frame), Some(client));
        assert!(frame.return_path.is_empty());
    }
}
