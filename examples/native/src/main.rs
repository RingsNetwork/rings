use std::sync::Arc;
use std::time::Duration;

use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::session::SessionSkBuilder;
use rings_core::storage::MemStorage;
use rings_node::extension::ext::Ctx;
use rings_node::extension::ext::Event;
use rings_node::extension::ext::Protocol;
use rings_node::extension::ext::Transition;
use rings_node::logging::init_logging;
use rings_node::logging::LogLevel;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use rings_rpc::method::Method;
use rings_rpc::protos::rings_node::*;

/// Namespace this example speaks over.
const EXAMPLE_NAMESPACE: &str = "example";

/// A minimal pure protocol for this demo: it logs each message it receives and replies
/// with nothing. Unlike the built-in `Echo`, it does not echo, so two peers both running
/// this example do not bounce a message back and forth forever.
struct Example;

impl Protocol for Example {
    type State = ();

    fn namespace(&self) -> &str {
        EXAMPLE_NAMESPACE
    }

    fn init(&self) {}

    fn step(&self, _ctx: Ctx<'_, ()>, event: &Event) -> Transition<()> {
        // `event.payload` is the raw bytes (the RPC boundary already base64-decoded it).
        println!(
            "<=== example protocol received from {}: {:?}",
            event.from,
            String::from_utf8_lossy(event.payload.as_ref())
        );
        Transition::pure(())
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
    let config = ProcessorConfig::new(0, "stun://stun.l.google.com:19302".to_string(), sk, 3);
    println!("===> Use network_id: 0");

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
    provider.register_protocol(Example).unwrap();

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

    // Wait for message sent.
    tokio::time::sleep(Duration::from_secs(3)).await;
}
