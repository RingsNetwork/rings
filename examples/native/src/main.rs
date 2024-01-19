use std::sync::Arc;

use async_trait::async_trait;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::message::MessagePayload;
use rings_core::session::SessionSkBuilder;
use rings_core::storage::MemStorage;
use rings_node::backend::types::BackendMessage;
use rings_node::backend::types::MessageHandler;
use rings_node::backend::Backend;
use rings_node::logging::init_logging;
use rings_node::logging::LogLevel;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use rings_rpc::method::Method;
use rings_rpc::protos::rings_node::*;

struct BackendBehaviour;

#[async_trait]
impl MessageHandler<BackendMessage> for BackendBehaviour {
    async fn handle_message(
        &self,
        _provider: Arc<Provider>,
        _ctx: &MessagePayload,
        msg: &BackendMessage,
    ) -> Result<(), Box<dyn std::error::Error>> {
        println!("Received message: {:?}", msg);
        Ok(())
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
    let config = ProcessorConfig::new("stun://stun.l.google.com:19302".to_string(), sk, 3);
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

    // Setup your callback handler.
    provider.set_backend_callback(BackendBehaviour).unwrap();

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
    println!("<=== ConnectPeerViaHttpResponse: {:?}", resp);

    let remote_did = resp.peer.unwrap().did;

    let connected = 'connected: {
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            println!("===> request ListPeers api...");
            let resp: ListPeersResponse = serde_json::from_value(
                provider
                    .request(Method::ListPeers, ListPeersRequest {})
                    .await
                    .unwrap(),
            )
            .unwrap();
            println!("<=== ListPeersResponse: {:?}", resp);

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

    let msg = BackendMessage::PlainText("Hello from native provider example".to_string());
    let rpc_req = msg
        .into_send_backend_message_request(destination_did)
        .unwrap();
    println!("===> request SendBackendMessage api...");
    let resp = provider
        .request(Method::SendBackendMessage, rpc_req)
        .await
        .unwrap();
    println!("<=== SendBackendMessage: {:?}", resp);

    // Wait for message sent.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
}
