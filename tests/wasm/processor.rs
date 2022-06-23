use std::sync::Arc;
use std::time::Duration;

use futures::lock::Mutex;
use rings_core::async_trait;
use rings_core::message::MessageCallback;
use rings_core::prelude::web3::contract::tokens::Tokenizable;
use rings_core::swarm::TransportManager;
use rings_node::browser::IntervalHandle;
use rings_node::prelude::rings_core;
use rings_node::prelude::wasm_bindgen::prelude::Closure;
use rings_node::prelude::wasm_bindgen_futures::spawn_local;
use rings_node::prelude::web_sys::window;
use rings_node::prelude::web_sys::RtcIceConnectionState;
use rings_node::prelude::*;
use rings_node::processor::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;
// wasm_bindgen_test_configure!(run_in_browser);

fn new_processor() -> Processor {
    let key = SecretKey::random();

    let (auth, new_key) = SessionManager::gen_unsign_info(key.address(), None, None).unwrap();
    let sig = key.sign(&auth.to_string().unwrap()).to_vec();
    let session = SessionManager::new(&sig, &auth, &new_key);
    let swarm = Arc::new(Swarm::new(
        "stun://stun.l.google.com:19302",
        key.address(),
        session,
    ));

    let dht = Arc::new(Mutex::new(PeerRing::new(key.address().into())));
    let msg_handler = MessageHandler::new(dht, swarm.clone());
    (swarm, Arc::new(msg_handler)).into()
}

fn listen(handler: &Arc<MessageHandler>) -> IntervalHandle {
    let h = Arc::clone(handler);

    let cb = Closure::wrap(Box::new(move || {
        let h1 = h.clone();
        spawn_local(Box::pin(async move {
            h1.clone().listen_once().await;
            // console_log!("listen_one call: {:?}", a);
        }));
    }) as Box<dyn FnMut()>);

    let interval_id = window()
        .unwrap()
        .set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 200)
        .ok()
        .unwrap();

    IntervalHandle::new(interval_id, cb)
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

    let (_transport_2, answer) = p2.answer_offer(offer.to_string().as_str()).await.unwrap();
    let peer = p1
        .accept_answer(
            transport_1.id.to_string().as_str(),
            answer.to_string().as_str(),
        )
        .await
        .unwrap();
    transport_1.wait_for_data_channel_open().await.unwrap();
    assert!(peer.transport.id.eq(&transport_1.id), "transport not same");
}

#[wasm_bindgen_test]
async fn test_processor_handshake_and_msg() {
    let p1 = new_processor();
    let p2 = new_processor();

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

    let p1_addr = p1.address().into_token().to_string();
    let p2_addr = p2.address().into_token().to_string();
    console_log!("p1_addr: {}", p1_addr);
    console_log!("p2_addr: {}", p2_addr);

    console_log!("listen");
    p1.msg_handler.set_callback(callback1).await;
    let interval1 = listen(&p1.msg_handler);

    p2.msg_handler.set_callback(callback2).await;
    let interval2 = listen(&p2.msg_handler);

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

    fluvio_wasm_timer::Delay::new(Duration::from_secs(2))
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
    drop(interval1);
    drop(interval2);
}

#[wasm_bindgen_test]
async fn test_processor_connect_with_address() {
    super::setup_log();
    let p1 = new_processor();
    console_log!("p1 address: {}", p1.address().into_token().to_string());
    let p2 = new_processor();
    console_log!("p2 address: {}", p2.address().into_token().to_string());
    let p3 = new_processor();
    console_log!("p3 address: {}", p3.address().into_token().to_string());

    let interval1 = listen(&p1.msg_handler);
    let interval2 = listen(&p2.msg_handler);
    let interval3 = listen(&p3.msg_handler);

    console_log!("connect p1 and p2");
    create_connection(&p1, &p2).await;
    console_log!("connect p1 and p2, done");
    console_log!("connect p2 and p3");
    create_connection(&p2, &p3).await;
    console_log!("connect p2 and p3, done");

    let p1_peers = p1.list_peers().await.unwrap();
    assert!(
        p1_peers.iter().any(|p| p
            .address
            .to_string()
            .eq(&p2.address().into_token().to_string())),
        "p2 not in p1's peer list"
    );

    fluvio_wasm_timer::Delay::new(Duration::from_secs(2))
        .await
        .unwrap();

    console_log!("connect p1 and p3");
    // p1 create connect with p3's address
    let peer3 = p1.connect_with_address(&p3.address(), true).await.unwrap();
    console_log!("transport connected");
    assert_eq!(
        peer3.transport.ice_connection_state().await.unwrap(),
        RtcIceConnectionState::Connected
    );

    let peers = p1.list_peers().await.unwrap();
    assert!(
        peers.iter().any(|p| p.address.to_string().eq(p3
            .address()
            .into_token()
            .to_string()
            .as_str())),
        "peer list dose NOT contains p3 address"
    );
    futures::join!(
        close_all_transport(&p1),
        close_all_transport(&p2),
        close_all_transport(&p3),
    );
    drop(interval1);
    drop(interval2);
    drop(interval3);
}
