use js_sys::Uint8Array;
use wasm_bindgen_test::*;

use crate::browser;
use crate::browser::Peer;
use crate::prelude::rings_core::utils;
use crate::prelude::rings_core::utils::js_value;
use crate::prelude::wasm_bindgen::convert::FromWasmAbi;
use crate::prelude::wasm_bindgen::JsValue;
use crate::prelude::wasm_bindgen_futures::JsFuture;
use crate::prelude::web3::contract::tokens::Tokenizable;
use crate::prelude::*;
wasm_bindgen_test_configure!(run_in_browser);

pub fn generic_of_jsval<T: FromWasmAbi<Abi = u32>>(
    js: JsValue,
    classname: &str,
) -> Result<T, JsValue> {
    use js_sys::Object;
    use js_sys::Reflect;
    let ctor_name = Object::get_prototype_of(&js).constructor().name();
    if ctor_name == classname {
        #[allow(unused_unsafe)]
        let ptr = unsafe { Reflect::get(&js, &JsValue::from_str("ptr"))? };
        let ptr_u32: u32 = ptr.as_f64().ok_or(JsValue::NULL)? as u32;
        let ge = unsafe { T::from_abi(ptr_u32) };
        Ok(ge)
    } else {
        Err(JsValue::NULL)
    }
}

async fn new_client() -> (browser::Client, String) {
    let key = SecretKey::random();
    let unsigned_info = browser::UnsignedInfo::new_with_signer(
        key.address().into_token().to_string(),
        browser::SignerMode::DEFAULT,
    )
    .ok()
    .unwrap();
    let auth = unsigned_info.auth().ok().unwrap();
    let signed_data = Uint8Array::from(key.sign(&auth).to_vec().as_slice());
    let stuns = "stun://stun.l.google.com:19302".to_owned();
    let storage_name = uuid::Uuid::new_v4().to_string();
    let c = JsFuture::from(browser::Client::new_client_with_storage(
        &unsigned_info,
        signed_data,
        stuns,
        storage_name.clone(),
    ))
    .await
    .ok()
    .unwrap();
    let client: browser::Client = generic_of_jsval(c, "Client").unwrap();
    (client, storage_name)
}

async fn create_connection(client1: &browser::Client, client2: &browser::Client) {
    let offer = JsFuture::from(client1.create_offer())
        .await
        .unwrap()
        .as_string()
        .unwrap();
    console_log!("offer: {:?}", offer);
    let answer = JsFuture::from(client2.answer_offer(offer))
        .await
        .ok()
        .unwrap()
        .as_string()
        .unwrap();
    console_log!("answer: {:?}", answer);
    JsFuture::from(client1.accept_answer(answer))
        .await
        .ok()
        .unwrap();
}

async fn get_peers(client: &browser::Client) -> Vec<browser::Peer> {
    let peers = JsFuture::from(client.list_peers()).await.ok().unwrap();
    let peers: js_sys::Array = peers.into();
    let peers: Vec<browser::Peer> = peers
        .iter()
        .flat_map(|x| js_value::deserialize(&x).ok())
        .collect::<Vec<_>>();
    peers
}

#[wasm_bindgen_test]
async fn test_two_client_connect_and_list() {
    // super::setup_log();
    let (client1, _storage1) = new_client().await;
    let (client2, _storage2) = new_client().await;

    futures::try_join!(
        JsFuture::from(client1.start()),
        JsFuture::from(client2.start()),
    )
    .unwrap();

    create_connection(&client1, &client2).await;
    console_log!("wait for register");
    utils::js_utils::window_sleep(1000).await.unwrap();

    let peers = get_peers(&client1).await;
    assert!(peers.len() == 1, "peers len should be 1");
    let peer2 = peers.get(0).unwrap();

    console_log!("get peer");
    let peer2: Peer = js_value::deserialize(
        &JsFuture::from(client1.get_peer(peer2.address.clone(), None))
            .await
            .unwrap(),
    )
    .unwrap();
    let peer2_state = JsFuture::from(client1.transport_state(peer2.address.clone(), None))
        .await
        .unwrap();
    assert!(
        peer2_state.eq("connected"),
        "peer2 state got {:?}",
        peer2_state,
    );

    JsFuture::from(client1.disconnect(peer2.address.clone(), None))
        .await
        .unwrap();
    let peers = get_peers(&client1).await;
    assert_eq!(peers.len(), 0);
}

#[wasm_bindgen_test]
async fn test_client_parse_params() {
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
