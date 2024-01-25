use std::sync::Arc;

use rings_rpc::protos::rings_node::*;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

use crate::backend::types::BackendMessage;
use crate::prelude::rings_core::utils;
use crate::prelude::rings_core::utils::js_value;
use crate::provider::browser;
use crate::provider::browser::Peer;
use crate::provider::Provider;
use crate::tests::wasm::prepare_processor;

wasm_bindgen_test_configure!(run_in_browser);

async fn new_provider() -> Provider {
    let processor = prepare_processor().await;
    Provider::from_processor(Arc::new(processor))
}

async fn get_peers(provider: &Provider) -> Vec<Peer> {
    let peers = JsFuture::from(provider.list_peers()).await.ok().unwrap();
    let peers: js_sys::Array = peers.into();
    let peers: Vec<Peer> = peers
        .iter()
        .flat_map(|x| js_value::deserialize(&x).ok())
        .collect::<Vec<_>>();
    peers
}

async fn create_connection(provider1: &Provider, provider2: &Provider) {
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

#[wasm_bindgen_test]
async fn test_two_provider_connect_and_list() {
    // super::setup_log();
    let provider1 = new_provider().await;
    let provider2 = new_provider().await;

    futures::try_join!(
        JsFuture::from(provider1.listen()),
        JsFuture::from(provider2.listen()),
    )
    .unwrap();

    create_connection(&provider1, &provider2).await;
    console_log!("wait for register");
    utils::js_utils::window_sleep(1000).await.unwrap();

    let peers = get_peers(&provider1).await;
    assert!(peers.len() == 1, "peers len should be 1");
    let peer2 = peers.first().unwrap();

    console_log!("get peer");
    let peer2: Peer = js_value::deserialize(
        &JsFuture::from(provider1.get_peer(peer2.did.clone(), None))
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        peer2.state.eq("Connected"),
        "peer2 state got {:?}",
        peer2.state,
    );

    JsFuture::from(provider1.disconnect(peer2.did.clone(), None))
        .await
        .unwrap();
    let peers = get_peers(&provider1).await;
    assert_eq!(peers.len(), 0);
}

#[wasm_bindgen_test]
async fn test_send_backend_message() {
    let provider1 = new_provider().await;
    let provider2 = new_provider().await;

    futures::try_join!(
        JsFuture::from(provider1.listen()),
        JsFuture::from(provider2.listen()),
    )
    .unwrap();

    create_connection(&provider1, &provider2).await;
    console_log!("wait for register");
    utils::js_utils::window_sleep(1000).await.unwrap();

    let msg = BackendMessage::PlainText("test".to_string());
    let req = msg
        .into_send_backend_message_request(provider2.address())
        .unwrap();

    JsFuture::from(provider1.request(
        "sendBackendMessage".to_string(),
        js_value::serialize(&req).unwrap(),
    ))
    .await
    .unwrap();
}

#[wasm_bindgen_test]
async fn test_get_address_from_hex_pubkey() {
    let pk = "02c0eeef8d136b10b862a0ac979eac2ad036f9902d87963ddf0fa108f1e275b9c7";

    let addr_result = browser::get_address_from_hex_pubkey(pk.to_string());
    assert!(addr_result.is_ok(), "addr_result is error");
    let addr = addr_result.ok().unwrap();
    assert!(
        addr.eq_ignore_ascii_case("0xfada88633e01d2f6704a7f2a6ebc57263aca6978"),
        "got addr {:?}",
        addr
    );
}

#[wasm_bindgen_test]
async fn test_get_address() {
    let expect_address = "0x8b98cf912975b4b6b67ce94882fc25c210a60a60";
    let got_address = browser::get_address(
        "9z1ZTaGocNSAu3DSqGKR6Dqt214X4dXucVd6C53EgqBK",
        browser::AddressType::Ed25519,
    )
    .ok()
    .unwrap();
    assert!(
        expect_address.eq_ignore_ascii_case(got_address.as_str()),
        "got address: {}, expect: {}",
        got_address,
        expect_address
    );
    let got_address = browser::get_address(expect_address, browser::AddressType::DEFAULT)
        .ok()
        .unwrap();

    assert!(
        got_address.eq_ignore_ascii_case(expect_address),
        "got address: {}, expect: {}",
        got_address,
        expect_address
    )
}
