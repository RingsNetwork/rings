use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

use crate::prelude::jsonrpc_core::types::response::Output;
use crate::prelude::jsonrpc_core::types::Value;
use crate::prelude::rings_core::prelude::uuid;
use crate::prelude::rings_core::utils;
use crate::prelude::rings_core::utils::js_value;
use crate::prelude::*;
use crate::processor::ProcessorConfig;
use crate::provider::browser;
use crate::provider::browser::Peer;
use crate::provider::Provider;

wasm_bindgen_test_configure!(run_in_browser);

async fn new_provider() -> (Provider, String) {
    let key = SecretKey::random();
    let sm = SessionSk::new_with_seckey(&key).unwrap();

    let config = serde_yaml::to_string(&ProcessorConfig::new(
        "stun://stun.l.google.com:19302".to_string(),
        sm,
        200,
    ))
    .unwrap();

    let storage_name = uuid::Uuid::new_v4().to_string();
    let provider: Provider = Provider::new_provider_with_storage_and_serialized_config_internal(
        config,
        storage_name.clone(),
    )
    .await
    .unwrap();
    (provider, storage_name)
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
    futures::try_join!(
        JsFuture::from(provider1.listen()),
        JsFuture::from(provider2.listen()),
    )
    .unwrap();

    let address = JsValue::from_str(&provider2.address());
    let req0 = js_sys::Array::of1(&address);
    let offer_fut = JsFuture::from(provider1.request("createOffer".to_string(), req0.into(), None))
        .await
        .unwrap();

    let offer: String =
        if let Output::Success(ret) = js_value::deserialize::<Output>(&offer_fut).unwrap() {
            if let Value::String(o) = ret.result {
                o
            } else {
                panic!("failed to get offer from output result {:?}", ret);
            }
        } else {
            panic!("request failed at create offer");
        };

    let js_offer = JsValue::from_str(&offer);
    let req1 = js_sys::Array::of1(&js_offer);
    let answer_fut =
        JsFuture::from(provider2.request("answerOffer".to_string(), req1.into(), None))
            .await
            .unwrap();

    let answer: String = match js_value::deserialize::<Output>(&answer_fut).unwrap() {
        Output::Success(ret) => {
            if let Value::String(o) = ret.result {
                o
            } else {
                panic!("failed to get answer from output result {:?}", ret);
            }
        }
        Output::Failure(e) => {
            panic!("request failed at accept offer, {:?}", e);
        }
    };

    let js_answer = JsValue::from_str(&answer);
    let req2 = js_sys::Array::of1(&js_answer);

    let _ret = JsFuture::from(provider1.request("acceptAnswer".to_string(), req2.into(), None))
        .await
        .unwrap();
}

#[wasm_bindgen_test]
async fn test_two_provider_connect_and_list() {
    // super::setup_log();
    let (provider1, _storage1) = new_provider().await;
    let (provider2, _storage2) = new_provider().await;

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
    let peer2 = peers.get(0).unwrap();

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
async fn test_provider_parse_params() {
    let null_value = browser::utils::parse_params(JsValue::null());
    assert!(null_value.is_ok(), "null_value is error");
    match null_value {
        Ok(v) => assert!(v == jsonrpc_core::Params::None, "not null"),
        Err(_) => panic!("err"),
    }

    let arr_v = js_sys::Array::new();
    arr_v.push(&JsValue::from_str("test1"));

    let jv: &JsValue = arr_v.as_ref();
    let value2 = browser::utils::parse_params(jv.clone()).unwrap();
    if let jsonrpc_core::Params::Array(v) = value2 {
        assert!(v.len() == 1, "value2.len got {}, expect 1", v.len());
        let v0 = v.get(0).unwrap();
        assert!(v0.is_string(), "v0 not string");
        assert!(v0.as_str() == Some("test1"), "v0 value {:?}", v0.as_str());
    } else {
        panic!("value2 not array");
    }
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

#[wasm_bindgen_test]
async fn test_create_connection_via_local_rpc() {
    // super::setup_log();
    let (provider1, _storage1) = new_provider().await;
    let (provider2, _storage2) = new_provider().await;

    futures::try_join!(
        JsFuture::from(provider1.listen()),
        JsFuture::from(provider2.listen()),
    )
    .unwrap();

    let address = JsValue::from_str(&provider2.address());
    let req0 = js_sys::Array::of1(&address);
    let offer_fut = JsFuture::from(provider1.request("createOffer".to_string(), req0.into(), None))
        .await
        .unwrap();

    let offer: String =
        if let Output::Success(ret) = js_value::deserialize::<Output>(&offer_fut).unwrap() {
            if let Value::String(o) = ret.result {
                o
            } else {
                panic!("failed to get offer from output result {:?}", ret);
            }
        } else {
            panic!("request failed at create offer");
        };

    let js_offer = JsValue::from_str(&offer);
    let req1 = js_sys::Array::of1(&js_offer);
    let answer_fut =
        JsFuture::from(provider2.request("answerOffer".to_string(), req1.into(), None))
            .await
            .unwrap();

    let answer: String = match js_value::deserialize::<Output>(&answer_fut).unwrap() {
        Output::Success(ret) => {
            if let Value::String(o) = ret.result {
                o
            } else {
                panic!("failed to get answer from output result {:?}", ret);
            }
        }
        Output::Failure(e) => {
            panic!("request failed at accept offer, {:?}", e);
        }
    };

    let js_answer = JsValue::from_str(&answer);
    let req2 = js_sys::Array::of1(&js_answer);

    let _ret = JsFuture::from(provider1.request("acceptAnswer".to_string(), req2.into(), None))
        .await
        .unwrap();
}
