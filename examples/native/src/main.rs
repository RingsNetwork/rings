use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::session::SessionSkBuilder;
use rings_core::storage::MemStorage;
use rings_node::extension::ext::Ctx;
use rings_node::extension::ext::Interpret;
use rings_node::extension::ext::Protocol;
use rings_node::extension::ext::Reject;
use rings_node::extension::ext::Scope;
use rings_node::extension::ext::Transition;
use rings_node::extension::ext::Wire;
use rings_node::logging::init_logging;
use rings_node::logging::LogLevel;
use rings_node::native::config::DEFAULT_NETWORK_ID;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use rings_rpc::method::Method;
use rings_rpc::protos::rings_node::*;

/// Namespace this example speaks over.
const EXAMPLE_NAMESPACE: &str = "example";
const REPLY_WINDOW_SECONDS: u64 = 30;

/// A decoded example message (who sent it + the text).
struct Received {
    summary: String,
}

/// The example's own effect: surface a received message. Printing is the *shell*'s job —
/// `step` stays pure (it only describes the effect).
enum ExampleEffect {
    Log(String),
}

/// A minimal pure protocol for this demo: on each message it emits a `Log` effect and
/// replies with nothing. Unlike the built-in `Echo` it does not echo, so two peers both
/// running this example do not bounce a message back and forth forever.
struct Example;

impl Protocol for Example {
    type State = ();
    type Event = Received;
    type Effect = ExampleEffect;

    fn namespace(&self) -> &str {
        EXAMPLE_NAMESPACE
    }

    fn init(&self) {}

    fn decode(&self, wire: Wire<'_>) -> Result<Received, Reject> {
        // `wire.payload` is the raw bytes (the RPC boundary already base64-decoded it).
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

/// The example's interpreter: the only place IO (here, printing) happens.
struct ExampleShell;

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

#[tokio::main]
async fn main() {
    init_logging(LogLevel::Info);

    // Generate a random secret key and its did.
    let key = SecretKey::random();
    let did = Did::from(key.address());

    let key_str = serde_json::to_string(&key).unwrap();
    println!("===> Current key: {key_str}"); // It's useful when you want to reproduce the same did.
    println!("===> Current did: {did}");

    // Build SessionSk of node in a safely way.
    // You can also use `SessionSk::new_with_key(&key)` directly.
    let mut skb = SessionSkBuilder::new(did.to_string(), "secp256k1".to_string());
    let sig = key.sign(&skb.unsigned_proof());
    skb = skb.set_session_sig(sig.to_vec());
    let sk = skb.build().unwrap();

    // Build processor
    let config = ProcessorConfig::new(
        DEFAULT_NETWORK_ID,
        "stun://stun.l.google.com:19302".to_string(),
        sk,
        3,
    );
    println!("===> Use network_id: {DEFAULT_NETWORK_ID}");

    let storage = Box::new(MemStorage::new());
    let processor = Arc::new(
        ProcessorBuilder::from_config(&config)
            .unwrap()
            .storage(storage)
            .build()
            .unwrap(),
    );

    // Wrap api with provider
    let provider = Arc::new(Provider::from_processor(processor));

    // Install the extension backend so inbound namespaced messages are dispatched to
    // registered protocols, then register this example's protocol so a peer running the
    // same binary has a handler for the `example` namespace (otherwise it would drop the
    // message as unknown).
    provider.set_backend().unwrap();
    provider.register_protocol(Example, ExampleShell).unwrap();

    // Listen messages from peers.
    let listening_provider = provider.clone();
    tokio::spawn(async move { listening_provider.listen().await });

    // Join remote network via url then send message to the did.
    let mut args: Vec<String> = std::env::args().rev().collect();
    let _ = args.pop();
    let url = args.pop().expect("remote address is required");
    let destination_did = args.pop().expect("did is required");

    println!("===> request ConnectPeerViaHttp api...");
    let resp: ConnectPeerViaHttpResponse = serde_json::from_value(
        provider
            .request(Method::ConnectPeerViaHttp, ConnectPeerViaHttpRequest {
                url,
            })
            .await
            .unwrap(),
    )
    .unwrap();
    println!("<=== ConnectPeerViaHttpResponse: {resp:?}");

    let remote_did = resp.did;

    let connected = 'connected: {
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_secs(1)).await;

            println!("===> request ListPeers api...");
            let resp: ListPeersResponse = serde_json::from_value(
                provider
                    .request(Method::ListPeers, ListPeersRequest {})
                    .await
                    .unwrap(),
            )
            .unwrap();
            println!("<=== ListPeersResponse: {resp:?}");

            if resp
                .peers
                .iter()
                .any(|peer| peer.did == remote_did && peer.state == "Connected")
            {
                break 'connected true;
            }
        }
        false
    };

    if !connected {
        panic!("Failed to connect to remote peer");
    }

    let rpc_req = SendBackendMessageRequest {
        destination_did,
        namespace: EXAMPLE_NAMESPACE.to_string(),
        // `data` is base64 on the wire (binary-safe); encode the raw message bytes.
        data: base64::encode(b"Hello from native provider example"),
    };
    println!("===> request SendBackendMessage api...");
    let resp = provider
        .request(Method::SendBackendMessage, rpc_req)
        .await
        .unwrap();
    println!("<=== SendBackendMessage: {resp:?}");

    println!("<=== waiting {REPLY_WINDOW_SECONDS}s for example replies...");
    tokio::time::sleep(Duration::from_secs(REPLY_WINDOW_SECONDS)).await;
}
