use std::sync::Arc;

use async_trait::async_trait;
use futures::channel::mpsc;
use futures::lock::Mutex;
use futures::StreamExt;
use rings_core::dht::Did;
use rings_core::swarm::callback::SwarmCallback;
use rings_core::swarm::callback::SwarmEvent;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_test::*;

use crate::prelude::*;
use crate::processor::*;
use crate::tests::wasm::await_mutual_admission;
use crate::tests::wasm::peer_transitions;
use crate::tests::wasm::prepare_processor;
use crate::tests::wasm::with_hang_guard;
use crate::tests::wasm::PeerTransitions;
use crate::tests::wasm::TransitionRecorder;
use crate::tests::wasm::TEST_HANG_GUARD;

async fn close_all_connections(p: &Processor) {
    futures::future::join_all(
        p.swarm
            .peers()
            .iter()
            .map(|peer| p.swarm.disconnect(peer.did.parse().unwrap())),
    )
    .await;
}

/// Swarm callback that forwards received custom texts and peer transitions to the test.
struct RecordingCallback {
    texts: mpsc::UnboundedSender<String>,
    transitions: TransitionRecorder,
}

#[async_trait(?Send)]
impl SwarmCallback for RecordingCallback {
    async fn on_inbound(
        &self,
        payload: &MessagePayload,
    ) -> Result<(), rings_core::error::CallbackError> {
        let msg: Message = payload.transaction.data().map_err(Box::new)?;
        if let Message::CustomMessage(ref msg) = msg {
            let text = String::from_utf8(msg.0.to_vec()).unwrap();
            console_log!("msg received: {}", text);
            self.texts.unbounded_send(text).unwrap();
        }
        Ok(())
    }

    async fn on_event(&self, event: &SwarmEvent) -> Result<(), rings_core::error::CallbackError> {
        self.transitions.record(event);
        Ok(())
    }
}

/// A processor whose callback records received texts and peer transitions from creation on.
struct ObservedProcessor {
    processor: Processor,
    transitions: PeerTransitions,
    texts: Mutex<mpsc::UnboundedReceiver<String>>,
}

impl ObservedProcessor {
    /// Build a processor and install its recording callback before any connection exists.
    async fn new() -> Self {
        let processor = prepare_processor().await;
        let (texts_sender, texts) = mpsc::unbounded();
        let (recorder, transitions) = peer_transitions();
        processor
            .swarm
            .set_callback(Arc::new(RecordingCallback {
                texts: texts_sender,
                transitions: recorder,
            }))
            .unwrap();
        Self {
            processor,
            transitions,
            texts: Mutex::new(texts),
        }
    }

    /// Await exactly `count` received texts, returned in sorted order.
    ///
    /// The channel buffers every text from creation on, so none can arrive unobserved.
    async fn receive_texts(&self, count: usize) -> Vec<String> {
        let mut texts = self.texts.lock().await;
        let mut received = texts.by_ref().take(count).collect::<Vec<_>>().await;
        assert_eq!(received.len(), count, "text channel closed early");
        received.sort();
        received
    }

    /// Await `peer`'s admission and return, in the same synchronous step, this processor's
    /// `peers()` entries for it.
    ///
    /// ```text
    /// loop:  consume Admitted(peer) ; is_peer_admitted(peer) ? return entries(peer)
    ///                                                        : consume Retired(peer)
    /// ```
    ///
    /// Concurrent connects from both ends may admit a generation that is superseded and
    /// retired right after. Its `Admitted` is then followed by a `Retired` in the log (the
    /// core law pairs them), so the loop consumes both and awaits the next admission. The
    /// browser runtime is single-threaded, and the check and the listing run with no await
    /// between them, so the entries returned belong to the announced generation.
    async fn settled_peer_states(&self, peer: Did) -> Vec<String> {
        loop {
            self.transitions.admitted(peer).await;
            if self.processor.swarm.is_peer_admitted(peer).unwrap() {
                return self.peer_states(peer);
            }
            self.transitions.retired(peer).await;
        }
    }

    /// `peers()` entries of this processor for `peer`.
    fn peer_states(&self, peer: Did) -> Vec<String> {
        let peer = peer.to_string();
        self.processor
            .swarm
            .peers()
            .into_iter()
            .filter(|inspect| inspect.did == peer)
            .map(|inspect| inspect.state)
            .collect()
    }
}

/// Connect `p1` to `p2` by offer and answer, and await both admissions.
///
/// `accept_answer` returns before the data channel opens and the peer joins the DHT; the
/// admission (`Connected`) event on each end is emitted exactly then.
async fn create_connection(p1: &ObservedProcessor, p2: &ObservedProcessor) {
    console_log!("create_offer");
    let offer = p1
        .processor
        .swarm
        .create_offer(p2.processor.did())
        .await
        .unwrap();

    console_log!("answer_offer");
    let answer = p2.processor.swarm.answer_offer(offer).await.unwrap();

    console_log!("accept_answer");
    p1.processor.swarm.accept_answer(answer).await.unwrap();

    await_mutual_admission(
        (p1.processor.did(), &p1.transitions),
        (p2.processor.did(), &p2.transitions),
    )
    .await;
}

/// Custom messages in both directions over one admitted link.
///
/// ```text
/// Admitted(p1, p2) ∧ Admitted(p2, p1) ; sends
///   ⊢  ◇(inbox(p1) = {2, 3, 5}) ∧ ◇(inbox(p2) = {1, 4})
/// ```
///
/// Each inbox is a channel buffered from creation on, and the test awaits exactly the expected
/// number of texts before it compares them as sorted multisets. Delivery order is not part of
/// the claim.
#[wasm_bindgen_test]
async fn test_processor_handshake_and_msg() {
    with_hang_guard("test_processor_handshake_and_msg", TEST_HANG_GUARD, async {
        let p1 = ObservedProcessor::new().await;
        let p2 = ObservedProcessor::new().await;

        let test_text1 = "test1";
        let test_text2 = "test2";
        let test_text3 = "test3";
        let test_text4 = "test4";
        let test_text5 = "test5";

        let p1_did = p1.processor.did();
        let p2_did = p2.processor.did();
        console_log!("p1_did: {}", p1_did);
        console_log!("p2_did: {}", p2_did);

        console_log!("listen");
        let p1_listener = p1.processor.clone();
        spawn_local(async move {
            p1_listener.listen().await;
        });
        let p2_listener = p2.processor.clone();
        spawn_local(async move {
            p2_listener.listen().await;
        });

        console_log!("processor_hs_connect_1_2");
        create_connection(&p1, &p2).await;

        console_log!("processor_send_test_text_messages");
        p1.processor
            .send_message(p2_did, test_text1.as_bytes())
            .await
            .unwrap();
        console_log!("send test_text1 done");

        p2.processor
            .send_message(p1_did, test_text2.as_bytes())
            .await
            .unwrap();
        console_log!("send test_text2 done");

        p2.processor
            .send_message(p1_did, test_text3.as_bytes())
            .await
            .unwrap();
        console_log!("send test_text3 done");

        p1.processor
            .send_message(p2_did, test_text4.as_bytes())
            .await
            .unwrap();
        console_log!("send test_text4 done");

        p2.processor
            .send_message(p1_did, test_text5.as_bytes())
            .await
            .unwrap();
        console_log!("send test_text5 done");

        console_log!("check received");
        let mut expect1 = vec![
            test_text2.to_owned(),
            test_text3.to_owned(),
            test_text5.to_owned(),
        ];
        expect1.sort();
        let mut expect2 = vec![test_text1.to_owned(), test_text4.to_owned()];
        expect2.sort();
        let (msgs1, msgs2) = futures::join!(
            p1.receive_texts(expect1.len()),
            p2.receive_texts(expect2.len()),
        );
        assert_eq!(msgs1, expect1);
        assert_eq!(msgs2, expect2);

        console_log!("processor_hs_close_all_connections");
        futures::join!(
            close_all_connections(&p1.processor),
            close_all_connections(&p2.processor),
        );
    })
    .await
}

/// A DHT-signalled connection between two peers that share only a relay.
///
/// ```text
/// Admitted(p1, p2) ∧ Admitted(p2, p3)
///   ; connect_with_did(p1 → p3) ∥ connect_with_did(p3 → p1)
///   ⊢ ◇Admitted(p1, p3) ∧ ◇Admitted(p3, p1)
/// ```
///
/// Admission means that the peer joined the local DHT, so after the two direct links p2 can
/// route p1's offer to p3 and p3's answer back. The concurrent connects may admit a
/// generation that is superseded right after; `settled_peer_states` reads the listing only
/// while the announced generation is current, and records are keyed by DID, so each end then
/// lists exactly one `Connected` entry for the other.
#[wasm_bindgen_test]
async fn test_processor_connect_with_did() {
    with_hang_guard("test_processor_connect_with_did", TEST_HANG_GUARD, async {
        super::setup_log();
        let p1 = ObservedProcessor::new().await;
        console_log!("p1 address: {}", p1.processor.did());
        let p2 = ObservedProcessor::new().await;
        console_log!("p2 address: {}", p2.processor.did());
        let p3 = ObservedProcessor::new().await;
        console_log!("p3 address: {}", p3.processor.did());

        console_log!("processor_connect_p1_and_p2");
        create_connection(&p1, &p2).await;
        console_log!("processor_connect_p1_and_p2, done");

        console_log!("processor_connect_p2_and_p3");
        create_connection(&p2, &p3).await;
        console_log!("processor_connect_p2_and_p3, done");

        assert_eq!(p1.peer_states(p2.processor.did()), ["Connected"]);

        console_log!("connect p1 and p3");
        let (p1_connect, p3_connect) = futures::join!(
            p1.processor.connect_with_did(p3.processor.did()),
            p3.processor.connect_with_did(p1.processor.did()),
        );
        p1_connect.unwrap();
        p3_connect.unwrap();
        let (p1_states, p3_states) = futures::join!(
            p1.settled_peer_states(p3.processor.did()),
            p3.settled_peer_states(p1.processor.did()),
        );

        console_log!("check peers");
        assert_eq!(p1_states, ["Connected"]);
        assert_eq!(p3_states, ["Connected"]);
        console_log!("processor_close_all_connections");
        futures::join!(
            close_all_connections(&p1.processor),
            close_all_connections(&p2.processor),
            close_all_connections(&p3.processor),
        );
    })
    .await
}
