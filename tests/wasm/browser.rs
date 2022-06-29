use js_sys::Uint8Array;
use rings_node::browser;
use rings_node::browser::Peer;
use rings_node::browser::SignerMode;
use rings_node::browser::TransportAndIce;
use rings_node::prelude::rings_core::prelude::web3::contract::tokens::Tokenizable;
use rings_node::prelude::wasm_bindgen_futures::JsFuture;
use rings_node::prelude::*;
use wasm_bindgen_test::*;
wasm_bindgen_test_configure!(run_in_browser);

#[allow(dead_code)]
fn new_client() -> browser::Client {
    let key = SecretKey::random();
    let unsigned_info = browser::UnsignedInfo::new_with_signer(
        key.address().into_token().to_string(),
        Some(SignerMode::DEFAULT),
    )
    .ok()
    .unwrap();
    let auth = unsigned_info.auth().ok().unwrap();
    let signed_data = Uint8Array::from(key.sign(&auth).to_vec().as_slice());
    let stuns = "stun://stun.l.google.com:19302".to_owned();
    browser::Client::new(&unsigned_info, signed_data, stuns)
        .ok()
        .unwrap()
}

async fn create_connection(client1: &browser::Client, client2: &browser::Client) {
    let offer = JsFuture::from(client1.create_offer()).await.unwrap();
    let offer: TransportAndIce = offer.into_serde().unwrap();
    let answer = JsFuture::from(client2.answer_offer(offer.ice()))
        .await
        .ok()
        .unwrap();
    console_log!("answer: {:?}", answer.as_string());
    let answer: TransportAndIce = answer.into_serde().unwrap();
    let _peer = JsFuture::from(client1.accept_answer(offer.transport_id(), answer.ice()))
        .await
        .ok()
        .unwrap();
}

async fn get_peers(client: &browser::Client) -> Vec<browser::Peer> {
    let peers = JsFuture::from(client.list_peers()).await.ok().unwrap();
    let peers: js_sys::Array = peers.into();
    let peers: Vec<browser::Peer> = peers
        .iter()
        .flat_map(|x| x.into_serde().ok())
        .collect::<Vec<_>>();
    peers
}

#[wasm_bindgen_test]
async fn test_two_client_connect_and_list() {
    // super::setup_log();
    let client1 = new_client();
    let client2 = new_client();

    futures::try_join!(
        JsFuture::from(client1.start()),
        JsFuture::from(client2.start()),
    )
    .unwrap();
    create_connection(&client1, &client2).await;
    let peers = get_peers(&client1).await;
    assert!(peers.len() == 1, "peers len should be 1");
    let peer1 = peers.get(0).unwrap();
    console_log!("wait for data channel open");
    JsFuture::from(client1.wait_for_data_channel_open(peer1.address()))
        .await
        .unwrap();
    console_log!("get peer");
    let peer1: Peer = JsFuture::from(client1.get_peer(peer1.address()))
        .await
        .unwrap()
        .into_serde()
        .unwrap();
    let peer1_state = JsFuture::from(client1.transport_state(peer1.address()))
        .await
        .unwrap();
    assert!(
        peer1_state.eq("connected"),
        "peer1 state got {:?}",
        peer1_state,
    );
    JsFuture::from(client1.disconnect(peer1.address()))
        .await
        .unwrap();
    let peers = get_peers(&client1).await;
    assert_eq!(peers.len(), 0);
}
