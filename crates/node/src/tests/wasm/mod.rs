pub mod browser;
pub mod processor;
pub mod snark;
use std::sync::Arc;

use rings_core::ecc::SecretKey;
use rings_core::prelude::uuid;
use rings_core::session::SessionSk;
use rings_core::storage::idb::IdbStorage;
use rings_rpc::protos::rings_node::AcceptAnswerRequest;
use rings_rpc::protos::rings_node::AnswerOfferRequest;
use rings_rpc::protos::rings_node::AnswerOfferResponse;
use rings_rpc::protos::rings_node::CreateOfferRequest;
use rings_rpc::protos::rings_node::CreateOfferResponse;
use wasm_bindgen_futures::JsFuture;

use crate::logging::browser::init_logging;
use crate::prelude::rings_core::utils::js_value;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;
use crate::provider::browser::Peer;
use crate::provider::Provider;

pub fn setup_log() {
    init_logging(crate::logging::LogLevel::Info);
    tracing::debug!("test")
}

pub async fn prepare_processor() -> Processor {
    let key = SecretKey::random();
    let sm = SessionSk::new_with_seckey(&key).unwrap();

    let config = serde_yaml::to_string(&ProcessorConfig::new(
        "stun://stun.l.google.com:19302".to_string(),
        sm,
        200,
    ))
    .unwrap();

    let storage_name = uuid::Uuid::new_v4().to_simple().to_string();
    let storage = Box::new(
        IdbStorage::new_with_cap_and_name(50000, &storage_name)
            .await
            .unwrap(),
    );

    ProcessorBuilder::from_serialized(&config)
        .unwrap()
        .storage(storage)
        .build()
        .unwrap()
}

pub async fn new_provider() -> Provider {
    let processor = prepare_processor().await;
    Provider::from_processor(Arc::new(processor))
}

pub async fn get_peers(provider: &Provider) -> Vec<Peer> {
    let peers = JsFuture::from(provider.list_peers()).await.ok().unwrap();
    let peers: js_sys::Array = peers.into();
    let peers: Vec<Peer> = peers
        .iter()
        .flat_map(|x| js_value::deserialize(&x).ok())
        .collect::<Vec<_>>();
    peers
}

pub async fn create_connection(provider1: &Provider, provider2: &Provider) {
    let req0 = CreateOfferRequest {
        did: provider2.address(),
    };
    let resp0 = JsFuture::from(provider1.request(
        "createOffer".to_string(),
        js_value::serialize(&req0).unwrap(),
    ))
    .await
    .unwrap();

    let offer = js_value::deserialize::<CreateOfferResponse>(resp0)
        .unwrap()
        .offer;

    let req1 = AnswerOfferRequest { offer };
    let resp1 = JsFuture::from(provider2.request(
        "answerOffer".to_string(),
        js_value::serialize(&req1).unwrap(),
    ))
    .await
    .unwrap();

    let answer = js_value::deserialize::<AnswerOfferResponse>(resp1)
        .unwrap()
        .answer;

    let req2 = AcceptAnswerRequest { answer };
    let _resp2 = JsFuture::from(provider1.request(
        "acceptAnswer".to_string(),
        js_value::serialize(&req2).unwrap(),
    ))
    .await
    .unwrap();
}
