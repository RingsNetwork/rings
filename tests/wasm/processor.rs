use std::sync::Arc;
use std::time::Duration;

use futures::lock::Mutex;
use rings_node::prelude::rings_core::async_trait;
use rings_node::prelude::rings_core::dht::Stabilization;
use rings_node::prelude::rings_core::dht::TStabilize;
use rings_node::prelude::rings_core::message::CallbackFn;
use rings_node::prelude::rings_core::message::MessageCallback;
use rings_node::prelude::rings_core::storage::PersistenceStorage;
use rings_node::prelude::rings_core::transports::manager::TransportManager;
use rings_node::prelude::web3::contract::tokens::Tokenizable;
use rings_node::prelude::web_sys::RtcIceConnectionState;
use rings_node::prelude::*;
use rings_node::processor::*;
use wasm_bindgen_test::*;

async fn new_processor() -> Processor {
    let key = SecretKey::random();

    let path = uuid::Uuid::new_v4().to_simple().to_string();
    console_log!("uuid: {}", path);
    let storage = PersistenceStorage::new_with_cap_and_name(1000, path.as_str())
        .await
        .unwrap();

    let swarm = Arc::new(
        SwarmBuilder::new("stun://stun.l.google.com:19302", storage)
            .key(key)
            .build()
            .unwrap(),
    );

    let msg_handler = Arc::new(swarm.message_handler(None, None));
    let stab = Arc::new(Stabilization::new(swarm.clone(), 20));

    (swarm, msg_handler, stab).into()
}

async fn listen(p: &Processor, cb: Option<CallbackFn>) {
    let h = Arc::new(p.swarm.message_handler(cb, None));
    let s = Arc::clone(&p.stabilization);

    futures::join!(
        async {
            h.listen().await;
        },
        async {
            s.wait().await;
        }
    );
}

async fn close_all_transport(p: &Processor) {
    futures::future::join_all(p.swarm.get_transports().iter().map(|(_, t)| t.close())).await;
}

struct MsgCallbackStruct {
    msgs: Arc<Mutex<Vec<String>>>,
}

#[async_trait(?Send)]
impl MessageCallback for MsgCallbackStruct {
    async fn custom_message(
        &self,
        handler: &MessageHandler,
        _ctx: &MessagePayload<Message>,
        msg: &MaybeEncrypted<CustomMessage>,
    ) {
        let msg = handler.decrypt_msg(msg).unwrap();
        let text = String::from_utf8(msg.0).unwrap();
        console_log!("msg received: {}", text);
        let mut msgs = self.msgs.try_lock().unwrap();
        msgs.push(text);
    }

    async fn builtin_message(&self, _handler: &MessageHandler, _ctx: &MessagePayload<Message>) {}
}

async fn create_connection(p1: &Processor, p2: &Processor) {
    let (transport_1, offer) = p1.create_offer().await.unwrap();
    let pendings_1 = p1.swarm.pending_transports().await.unwrap();
    assert_eq!(pendings_1.len(), 1);
    assert_eq!(
        pendings_1.get(0).unwrap().id.to_string(),
        transport_1.id.to_string()
    );

    let (transport_2, answer) = p2.answer_offer(offer.to_string().as_str()).await.unwrap();
    let peer = p1
        .accept_answer(
            transport_1.id.to_string().as_str(),
            answer.to_string().as_str(),
        )
        .await
        .unwrap();
    assert!(peer.transport.id.eq(&transport_1.id), "transport not same");
    futures::try_join!(
        async { transport_1.wait_for_data_channel_open().await },
        async { transport_2.wait_for_data_channel_open().await }
    )
    .unwrap();
}

#[wasm_bindgen_test]
async fn test_processor_handshake_and_msg() {
    let p1 = new_processor().await;
    let p2 = new_processor().await;

    create_connection(&p1, &p2).await;

    let msgs1: Arc<Mutex<Vec<String>>> = Default::default();
    let msgs2: Arc<Mutex<Vec<String>>> = Default::default();
    let callback1 = Box::new(MsgCallbackStruct {
        msgs: msgs1.clone(),
    });
    let callback2 = Box::new(MsgCallbackStruct {
        msgs: msgs2.clone(),
    });

    let test_text1 = "test1";
    let test_text2 = "test2";
    let test_text3 = "test3";
    let test_text4 = "test4";
    let test_text5 = "test5";

    let p1_addr = p1.did().into_token().to_string();
    let p2_addr = p2.did().into_token().to_string();
    console_log!("p1_addr: {}", p1_addr);
    console_log!("p2_addr: {}", p2_addr);

    console_log!("listen");
    listen(&p1, Some(callback1)).await;
    listen(&p2, Some(callback2)).await;

    p1.send_message(p2_addr.as_str(), test_text1.as_bytes())
        .await
        .unwrap();
    console_log!("send test_text1 done");

    p2.send_message(p1_addr.as_str(), test_text2.as_bytes())
        .await
        .unwrap();
    console_log!("send test_text2 done");

    p2.send_message(p1_addr.as_str(), test_text3.as_bytes())
        .await
        .unwrap();
    console_log!("send test_text3 done");

    p1.send_message(p2_addr.as_str(), test_text4.as_bytes())
        .await
        .unwrap();
    console_log!("send test_text4 done");

    p2.send_message(p1_addr.as_str(), test_text5.as_bytes())
        .await
        .unwrap();
    console_log!("send test_text5 done");

    fluvio_wasm_timer::Delay::new(Duration::from_secs(4))
        .await
        .unwrap();

    console_log!("check received");

    let mut msgs1 = msgs1.try_lock().unwrap().as_slice().to_vec();
    msgs1.sort();
    let mut msgs2 = msgs2.try_lock().unwrap().as_slice().to_vec();
    msgs2.sort();

    let mut expect1 = vec![
        test_text2.to_owned(),
        test_text3.to_owned(),
        test_text5.to_owned(),
    ];
    expect1.sort();

    let mut expect2 = vec![test_text1.to_owned(), test_text4.to_owned()];
    expect2.sort();
    assert_eq!(msgs1, expect1);
    assert_eq!(msgs2, expect2);

    futures::join!(close_all_transport(&p1), close_all_transport(&p2),);
}

#[wasm_bindgen_test]
async fn test_processor_connect_with_did() {
    super::setup_log();
    let p1 = new_processor().await;
    console_log!("p1 address: {}", p1.did());
    let p2 = new_processor().await;
    console_log!("p2 address: {}", p2.did());
    let p3 = new_processor().await;
    console_log!("p3 address: {}", p3.did());

    listen(&p1, None).await;
    listen(&p2, None).await;
    listen(&p3, None).await;

    console_log!("connect p1 and p2");
    create_connection(&p1, &p2).await;
    console_log!("connect p1 and p2, done");
    console_log!("connect p2 and p3");
    create_connection(&p2, &p3).await;
    console_log!("connect p2 and p3, done");

    let p1_peers = p1.list_peers().await.unwrap();
    assert!(
        p1_peers
            .iter()
            .any(|p| p.did.to_string().eq(&p2.did().into_token().to_string())),
        "p2 not in p1's peer list"
    );

    fluvio_wasm_timer::Delay::new(Duration::from_secs(2))
        .await
        .unwrap();

    console_log!("connect p1 and p3");
    // p1 create connect with p3's address
    let peer3 = p1.connect_with_did(p3.did(), true).await.unwrap();
    console_log!("transport connected");
    fluvio_wasm_timer::Delay::new(Duration::from_millis(1000))
        .await
        .unwrap();
    assert_eq!(
        peer3.transport.ice_connection_state().await.unwrap(),
        RtcIceConnectionState::Connected
    );

    let peers = p1.list_peers().await.unwrap();
    assert!(
        peers.iter().any(|p| p
            .did
            .to_string()
            .eq(p3.did().into_token().to_string().as_str())),
        "peer list dose NOT contains p3 address"
    );
    futures::join!(
        close_all_transport(&p1),
        close_all_transport(&p2),
        close_all_transport(&p3),
    );
}
