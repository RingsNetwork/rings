pub mod test_browser;
pub mod test_evidence;
pub mod test_processor;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::future::Either;
use futures::lock::Mutex;
use futures::StreamExt;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::storage::idb::IdbStorage;
use rings_core::swarm::callback::PeerTransition;
use rings_core::swarm::callback::SwarmEvent;
use rings_rpc::protos::rings_node::*;
use uuid;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::wasm_bindgen_test_configure;

use crate::extension::Backend;
use crate::extension::BackendObserver;
use crate::logging::browser::init_logging;
use crate::prelude::rings_core::utils::js_value;
use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;
use crate::provider::Provider;

const TEST_DHT_FINGER_TABLE_SIZE: usize = 8;

/// ICE servers of every browser fixture: none, so peers gather host candidates only.
///
/// Every peer of these tests lives in the same page. An external STUN server would put its
/// latency inside `createOffer`/`answerOffer`, which wait for ICE gathering to complete, and
/// so ahead of every awaited event.
const TEST_ICE_SERVERS: &str = "";

/// Per-test hang guard of the connection tests; see [`with_hang_guard`].
///
/// Budget arithmetic: wasm-bindgen-test gives the whole binary 120 s. The suite passes in
/// about 9 s, so one test that hangs to this guard still leaves the rest of the binary its
/// normal runtime within the budget, and the named guard fires before the runner's.
pub const TEST_HANG_GUARD: Duration = Duration::from_secs(30);

/// A logical peer transition, as [`SwarmEvent::peer_transition`] reads it off the event stream.
type Transition = (Did, PeerTransition);

/// Write side of a peer-transition log: forwards every admission and retirement of one swarm.
///
/// Install it when the swarm is created, before any connection exists. Every transition is
/// then buffered in the log, so a test that awaits one later cannot miss it.
#[derive(Clone)]
pub struct TransitionRecorder(mpsc::UnboundedSender<Transition>);

impl TransitionRecorder {
    /// Record the transition `event` carries, if any.
    pub fn record(&self, event: &SwarmEvent) {
        if let Some(transition) = event.peer_transition() {
            self.send(transition);
        }
    }

    /// Append `transition` to the log; a log whose reader is gone has nobody left to wake.
    fn send(&self, transition: Transition) {
        let _ = self.0.unbounded_send(transition);
    }
}

impl BackendObserver for TransitionRecorder {
    fn lookup_report(&self, _tx_id: uuid::Uuid, _successor: Did) {}

    fn peer_admitted(&self, peer: Did) {
        self.send((peer, PeerTransition::Admitted));
    }

    fn peer_retired(&self, peer: Did) {
        self.send((peer, PeerTransition::Retired));
    }
}

/// Read side of a peer-transition log.
pub struct PeerTransitions(Mutex<TransitionLog>);

/// Transitions received so far that no wait has consumed, and the channel of later ones.
struct TransitionLog {
    unconsumed: Vec<Transition>,
    receiver: mpsc::UnboundedReceiver<Transition>,
}

/// A peer-transition log: the recorder to install on a swarm and the reader a test awaits.
pub fn peer_transitions() -> (TransitionRecorder, PeerTransitions) {
    let (sender, receiver) = mpsc::unbounded();
    (
        TransitionRecorder(sender),
        PeerTransitions(Mutex::new(TransitionLog {
            unconsumed: Vec::new(),
            receiver,
        })),
    )
}

impl PeerTransitions {
    /// Await `peer`'s next admission: its transport is ready and it joined the local DHT.
    pub async fn admitted(&self, peer: Did) {
        self.consume((peer, PeerTransition::Admitted)).await
    }

    /// Await `peer`'s next retirement: its record left the local DHT.
    pub async fn retired(&self, peer: Did) {
        self.consume((peer, PeerTransition::Retired)).await
    }

    /// Consume the oldest unconsumed occurrence of `wanted`, waiting for it if necessary.
    ///
    /// ```text
    /// log = unconsumed ++ channel        (arrival order)
    /// consume(t): remove the first t in log, awaiting the channel until one arrives
    /// ```
    ///
    /// Law (no lost wake-up): the recorder was installed before any connection existed, and
    /// the channel is unbounded, so every transition that occurred is in `log`, whether it
    /// happened before or after this call. Transitions skipped on the way stay in
    /// `unconsumed` for later waits, so the order in which a test waits is free.
    async fn consume(&self, wanted: Transition) {
        let mut log = self.0.lock().await;
        if let Some(position) = log.unconsumed.iter().position(|seen| *seen == wanted) {
            log.unconsumed.remove(position);
            return;
        }
        let arrival = async {
            while let Some(transition) = log.receiver.next().await {
                if transition == wanted {
                    return true;
                }
                log.unconsumed.push(transition);
            }
            false
        };
        if !arrival.await {
            panic!("transition recorder dropped before {wanted:?}");
        }
    }
}

/// Await `a`'s admission of `b` and `b`'s admission of `a`.
pub async fn await_mutual_admission(
    (a, a_transitions): (Did, &PeerTransitions),
    (b, b_transitions): (Did, &PeerTransitions),
) {
    futures::join!(a_transitions.admitted(b), b_transitions.admitted(a));
}

/// Run the test `name` under a per-test hang guard of `budget`.
///
/// The binary's tests share one runner budget, so a hung test would otherwise time out the
/// whole binary and starve every test after it. The guard fails with `name` instead. It is a
/// failure bound only: a passing run proceeds on `test` alone, since every wait inside is on
/// an event.
pub async fn with_hang_guard<T>(
    name: &str,
    budget: Duration,
    test: impl std::future::Future<Output = T>,
) -> T {
    match futures::future::select(Box::pin(test), Box::pin(rings_runtime::sleep(budget))).await {
        Either::Left((value, _)) => value,
        Either::Right(_) => panic!("{name} exceeded its {budget:?} hang guard"),
    }
}

wasm_bindgen_test_configure!(run_in_browser);

pub fn setup_log() {
    init_logging(crate::logging::LogLevel::Info);
    tracing::debug!("test")
}

pub async fn prepare_processor() -> Processor {
    let key = SecretKey::random();
    let sm = DelegateeKey::new_with_seckey(&key).unwrap();

    let config = serde_yaml::to_string(&ProcessorConfig::new(
        0,
        TEST_ICE_SERVERS.to_string(),
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
        .dht_finger_table_size(TEST_DHT_FINGER_TABLE_SIZE)
        .build()
        .unwrap()
}

/// A provider together with the log of its peer transitions.
pub struct ObservedProvider {
    /// The provider under test.
    pub provider: Provider,
    /// Admissions and retirements its extension backend observed.
    pub transitions: PeerTransitions,
}

/// A provider whose extension backend records its peer transitions from creation on.
pub async fn new_provider() -> ObservedProvider {
    let processor = prepare_processor().await;
    let provider = Provider::from_processor(Arc::new(processor));
    let (recorder, transitions) = peer_transitions();
    let backend = Backend::new(Arc::new(provider.clone())).observed_by(Arc::new(recorder));
    provider
        .set_swarm_callback_internal(Arc::new(backend))
        .unwrap();
    ObservedProvider {
        provider,
        transitions,
    }
}

/// The DID `provider` signs as.
pub fn provider_did(provider: &Provider) -> Did {
    provider.address().parse().unwrap()
}

pub async fn get_peers(provider: &Provider) -> Vec<PeerInfo> {
    let resp = JsFuture::from(provider.request(
        "listPeers".to_string(),
        js_value::serialize(&ListPeersRequest {}).unwrap(),
    ))
    .await
    .unwrap();

    js_value::deserialize::<ListPeersResponse>(resp)
        .unwrap()
        .peers
}

/// Connect two providers and await both admissions.
///
/// The offer/answer exchange returns before ICE, DTLS, the data-channel open and core
/// admission complete, so the admission events on both ends are the completion signal.
pub async fn create_connection(node1: &ObservedProvider, node2: &ObservedProvider) {
    let req0 = CreateOfferRequest {
        did: node2.provider.address(),
    };
    let resp0 = JsFuture::from(node1.provider.request(
        "createOffer".to_string(),
        js_value::serialize(&req0).unwrap(),
    ))
    .await
    .unwrap();

    let offer = js_value::deserialize::<CreateOfferResponse>(resp0)
        .unwrap()
        .offer;

    let req1 = AnswerOfferRequest { offer };
    let resp1 = JsFuture::from(node2.provider.request(
        "answerOffer".to_string(),
        js_value::serialize(&req1).unwrap(),
    ))
    .await
    .unwrap();

    let answer = js_value::deserialize::<AnswerOfferResponse>(resp1)
        .unwrap()
        .answer;

    let req2 = AcceptAnswerRequest { answer };
    let _resp2 = JsFuture::from(node1.provider.request(
        "acceptAnswer".to_string(),
        js_value::serialize(&req2).unwrap(),
    ))
    .await
    .unwrap();

    await_mutual_admission(
        (provider_did(&node1.provider), &node1.transitions),
        (provider_did(&node2.provider), &node2.transitions),
    )
    .await;
}
