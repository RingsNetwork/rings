use std::sync::Arc;
use std::time::Duration;

use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::storage::MemStorage;
use rings_native_example::build_session_key;
use rings_native_example::example_message_request;
use rings_native_example::parse_cli_args;
use rings_native_example::peer_is_connected;
use rings_native_example::Example;
use rings_native_example::ExampleShell;
use rings_native_example::REPLY_WINDOW_SECONDS;
use rings_node::logging::init_logging;
use rings_node::logging::LogLevel;
use rings_node::native::config::DEFAULT_NETWORK_ID;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use rings_rpc::method::Method;
use rings_rpc::protos::rings_node::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging(LogLevel::Info);

    // Generate a random secret key and its did.
    let key = SecretKey::random();
    let did = Did::from(key.address());

    let key_str = serde_json::to_string(&key)?;
    println!("===> Current key: {key_str}"); // It's useful when you want to reproduce the same did.
    println!("===> Current did: {did}");

    // Build SessionSk of node in a safely way.
    // You can also use `SessionSk::new_with_key(&key)` directly.
    let sk = build_session_key(&key)?;

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
        ProcessorBuilder::from_config(&config)?
            .storage(storage)
            .build()?,
    );

    // Wrap api with provider
    let provider = Arc::new(Provider::from_processor(processor));

    // Install the extension backend so inbound namespaced messages are dispatched to
    // registered protocols, then register this example's protocol so a peer running the
    // same binary has a handler for the `example` namespace (otherwise it would drop the
    // message as unknown).
    provider.set_backend()?;
    provider.register_protocol(Example, ExampleShell)?;

    // Listen messages from peers.
    let listening_provider = provider.clone();
    tokio::spawn(async move { listening_provider.listen().await });

    // Join remote network via url then send message to the did.
    let args = parse_cli_args(std::env::args())?;

    println!("===> request ConnectPeerViaHttp api...");
    let resp: ConnectPeerViaHttpResponse = serde_json::from_value(
        provider
            .request(Method::ConnectPeerViaHttp, ConnectPeerViaHttpRequest {
                url: args.seed_url,
            })
            .await?,
    )?;
    println!("<=== ConnectPeerViaHttpResponse: {resp:?}");

    let remote_did = resp.did;

    let connected = 'connected: {
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_secs(1)).await;

            println!("===> request ListPeers api...");
            let resp: ListPeersResponse = serde_json::from_value(
                provider
                    .request(Method::ListPeers, ListPeersRequest {})
                    .await?,
            )?;
            println!("<=== ListPeersResponse: {resp:?}");

            if peer_is_connected(&resp.peers, &remote_did) {
                break 'connected true;
            }
        }
        false
    };

    if !connected {
        return Err("failed to connect to remote peer".into());
    }

    let rpc_req = example_message_request(args.destination_did);
    println!("===> request SendBackendMessage api...");
    let resp = provider
        .request(Method::SendBackendMessage, rpc_req)
        .await?;
    println!("<=== SendBackendMessage: {resp:?}");

    println!("<=== waiting {REPLY_WINDOW_SECONDS}s for example replies...");
    tokio::time::sleep(Duration::from_secs(REPLY_WINDOW_SECONDS)).await;
    Ok(())
}
