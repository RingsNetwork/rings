use std::fmt;

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::message::e2e;
use rings_core::session::SessionSk;
use rings_core::session::SessionSkBuilder;
use rings_node::extension::ext::Ctx;
use rings_node::extension::ext::Interpret;
use rings_node::extension::ext::Protocol;
use rings_node::extension::ext::Reject;
use rings_node::extension::ext::Scope;
use rings_node::extension::ext::Transition;
use rings_node::extension::ext::Wire;
use rings_rpc::protos::rings_node::PeerInfo;
use rings_rpc::protos::rings_node::SendBackendMessageRequest;

/// Namespace this example speaks over.
pub const EXAMPLE_NAMESPACE: &str = "example";

/// How long the runnable example waits for replies after sending.
pub const REPLY_WINDOW_SECONDS: u64 = 30;

/// Parsed arguments for the runnable native example.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExampleArgs {
    /// Seed node HTTP URL.
    pub seed_url: String,
    /// Destination DID that receives the example message.
    pub destination_did: String,
}

/// CLI argument parsing error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExampleArgsError {
    /// The seed URL argument is missing.
    MissingSeedUrl,
    /// The destination DID argument is missing.
    MissingDestinationDid,
    /// More arguments were supplied than the example accepts.
    UnexpectedArgument(String),
}

impl fmt::Display for ExampleArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSeedUrl => write!(f, "remote seed URL is required"),
            Self::MissingDestinationDid => write!(f, "destination DID is required"),
            Self::UnexpectedArgument(arg) => write!(f, "unexpected argument: {arg}"),
        }
    }
}

impl std::error::Error for ExampleArgsError {}

/// A decoded example message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    /// Printable summary of the sender and payload.
    pub summary: String,
}

/// The example's own effect: surface a received message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExampleEffect {
    /// Log a received message summary.
    Log(String),
}

/// A minimal pure protocol for this demo.
pub struct Example;

impl Protocol for Example {
    type State = ();
    type Event = Received;
    type Effect = ExampleEffect;

    fn namespace(&self) -> &str {
        EXAMPLE_NAMESPACE
    }

    fn init(&self) {}

    fn decode(&self, wire: Wire<'_>) -> Result<Received, Reject> {
        Ok(Received {
            summary: format!(
                "from {}: {:?}",
                wire.from,
                String::from_utf8_lossy(wire.payload)
            ),
        })
    }

    fn step(&self, _ctx: Ctx<'_, ()>, event: Received) -> Transition<(), ExampleEffect> {
        Transition::with((), vec![ExampleEffect::Log(event.summary)])
    }
}

/// The example's interpreter: the only place IO happens.
pub struct ExampleShell;

#[async_trait::async_trait]
impl Interpret for ExampleShell {
    type Effect = ExampleEffect;

    async fn run(
        &self,
        _scope: &Scope,
        effect: ExampleEffect,
    ) -> rings_node::error::Result<Vec<Bytes>> {
        match effect {
            ExampleEffect::Log(summary) => {
                println!("<=== example protocol received {summary}");
                Ok(Vec::new())
            }
        }
    }
}

/// Parse the runnable example's command line.
pub fn parse_cli_args<I, S>(args: I) -> Result<ExampleArgs, ExampleArgsError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    let _program = args.next();
    let seed_url = args.next().ok_or(ExampleArgsError::MissingSeedUrl)?;
    let destination_did = args.next().ok_or(ExampleArgsError::MissingDestinationDid)?;
    if let Some(extra) = args.next() {
        return Err(ExampleArgsError::UnexpectedArgument(extra));
    }
    Ok(ExampleArgs {
        seed_url,
        destination_did,
    })
}

/// Build a session key for the randomly generated account.
pub fn build_session_key(key: &SecretKey) -> rings_core::error::Result<SessionSk> {
    let did = Did::from(key.address());
    let mut builder = SessionSkBuilder::new(did.to_string(), "secp256k1".to_string());
    let sig = key.sign(&builder.unsigned_proof());
    builder = builder.set_session_sig(sig.to_vec());
    builder.build()
}

/// Return whether the list-peers response contains a connected remote peer.
pub fn peer_is_connected(peers: &[PeerInfo], remote_did: &str) -> bool {
    peers
        .iter()
        .any(|peer| peer.did == remote_did && peer.state == "Connected")
}

/// Build the example backend-message RPC request.
pub fn example_message_request(destination_did: String) -> SendBackendMessageRequest {
    SendBackendMessageRequest {
        destination_did,
        namespace: EXAMPLE_NAMESPACE.to_string(),
        data: base64::encode(b"Hello from native provider example"),
    }
}

/// Exercise the example's E2E encryption model with real identity keys.
///
/// This is the same direct-ElGamal stream model used by [`rings_node::processor::Processor`]:
/// the sender encrypts to the recipient identity public key, and the recipient decrypts
/// with its identity secret key.
pub fn e2e_example_round_trip(
    sender: &SecretKey,
    recipient: SecretKey,
    plaintext: &[u8],
    max_plaintext_frame_len: usize,
) -> rings_core::error::Result<Vec<u8>> {
    let stream_id = rings_core::prelude::uuid::Uuid::new_v4();
    let frames = e2e::encrypt_stream_frames(
        plaintext,
        stream_id,
        sender.pubkey(),
        recipient.pubkey(),
        max_plaintext_frame_len,
    )?
    .collect::<rings_core::error::Result<Vec<_>>>()?;
    e2e::decrypt_stream(&frames, stream_id, Did::from(sender.address()), recipient)
}

#[cfg(test)]
mod tests {
    use rings_core::dht::Did;
    use rings_core::ecc::SecretKey;
    use rings_node::extension::ext::Ctx;
    use rings_node::extension::ext::Protocol;
    use rings_node::extension::ext::Wire;

    use super::*;

    #[test]
    fn parse_cli_args_requires_seed_and_destination() {
        assert_eq!(
            parse_cli_args(["rings-native-example"]),
            Err(ExampleArgsError::MissingSeedUrl)
        );
        assert_eq!(
            parse_cli_args(["rings-native-example", "http://127.0.0.1:50001"]),
            Err(ExampleArgsError::MissingDestinationDid)
        );
    }

    #[test]
    fn parse_cli_args_accepts_exact_seed_and_destination() {
        let args = parse_cli_args([
            "rings-native-example",
            "http://127.0.0.1:50001",
            "0x11E807fcc88dD319270493fB2e822e388Fe36ab0",
        ])
        .expect("valid args");

        assert_eq!(args.seed_url, "http://127.0.0.1:50001");
        assert_eq!(
            args.destination_did,
            "0x11E807fcc88dD319270493fB2e822e388Fe36ab0"
        );
    }

    #[test]
    fn peer_is_connected_requires_matching_did_and_connected_state() {
        let peers = vec![
            PeerInfo {
                did: "0xabc".to_string(),
                state: "Connecting".to_string(),
            },
            PeerInfo {
                did: "0xdef".to_string(),
                state: "Connected".to_string(),
            },
        ];

        assert!(!peer_is_connected(&peers, "0xabc"));
        assert!(peer_is_connected(&peers, "0xdef"));
    }

    #[test]
    fn example_message_request_is_base64_encoded_for_the_example_namespace() {
        let req = example_message_request("0xdef".to_string());

        assert_eq!(req.destination_did, "0xdef");
        assert_eq!(req.namespace, EXAMPLE_NAMESPACE);
        assert_eq!(
            base64::decode(req.data).expect("base64"),
            b"Hello from native provider example"
        );
    }

    #[test]
    fn protocol_logs_sender_and_payload_without_replying() {
        let did = Did::from(SecretKey::random().address());
        let protocol = Example;
        let event = protocol
            .decode(Wire {
                from: did,
                me: did,
                payload: b"hello",
            })
            .expect("decode");
        let transition = protocol.step(Ctx { did, state: &() }, event);

        assert_eq!(transition.effects.len(), 1);
        match transition.effects.first().expect("one log effect") {
            ExampleEffect::Log(summary) => {
                assert!(summary.contains(&did.to_string()));
                assert!(summary.contains("hello"));
            }
        }
    }

    #[test]
    fn build_session_key_uses_the_generated_account_did() {
        let key = SecretKey::random();
        let did = Did::from(key.address());
        let session = build_session_key(&key).expect("session key");

        assert_eq!(session.account_did(), did);
    }

    #[test]
    fn e2e_example_round_trip_decrypts_direct_elgamal_stream() {
        let sender = SecretKey::random();
        let recipient = SecretKey::random();
        let plaintext = b"native example encrypted e2e body";
        let decrypted = e2e_example_round_trip(&sender, recipient, plaintext, 8).expect("e2e");

        assert_eq!(decrypted, plaintext);
    }
}
