use std::sync::Arc;

use async_trait::async_trait;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::message::MessagePayload;
use rings_core::session::SessionSkBuilder;
use rings_core::storage::PersistenceStorage;
use rings_node::backend::types::BackendMessage;
use rings_node::backend::types::MessageHandler;
use rings_node::backend::Backend;
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
    // Generate a random secret key and its did.
    let key = SecretKey::random();
    let did = Did::from(key.address());

    // Build SessionSk of node in a safely way.
    // You can also use `SessionSk::new_with_key(&key)` directly.
    let mut skb = SessionSkBuilder::new(did.to_string(), "secp256k1".to_string());
    let sig = key.sign(&skb.unsigned_proof());
    skb = skb.set_session_sig(sig.to_vec());
    let sk = skb.build().unwrap();

    // Build processor
    let config = ProcessorConfig::new("stun://stun.l.google.com:19302".to_string(), sk, 3);
    let storage_path = PersistenceStorage::random_path("./tmp");
    let storage = PersistenceStorage::new_with_path(storage_path.as_str())
        .await
        .unwrap();
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
    let backend = Arc::new(Backend::new(provider.clone(), Box::new(BackendBehaviour)));
    provider.set_swarm_callback(backend).unwrap();

    // Listen messages from peers.
    let listening_provider = provider.clone();
    tokio::spawn(async move { listening_provider.listen().await });

    // Invoke apis of node.
    println!("\nrequest NodeInfo api...");
    let resp = provider
        .request(Method::NodeInfo, NodeInfoRequest {})
        .await
        .unwrap();
    println!("NodeInfo: {:?}", resp);

    println!("\nrequest CreateOffer api...");
    let resp = provider
        .request(Method::CreateOffer, CreateOfferRequest {
            did: "0x11E807fcc88dD319270493fB2e822e388Fe36ab0".to_string(),
        })
        .await
        .unwrap();
    println!("CreateOffer: {:?}", resp);
}
