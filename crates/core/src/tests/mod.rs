use crate::swarm::Swarm;

#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(not(feature = "wasm"))]
pub mod default;

#[allow(dead_code)]
pub fn setup_tracing() {
    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(tracing::Level::DEBUG)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
}

pub async fn manually_establish_connection(swarm1: &Swarm, swarm2: &Swarm) {
    assert!(swarm1.transport.get_connection(swarm2.did()).is_none());
    assert!(swarm2.transport.get_connection(swarm1.did()).is_none());

    let offer = swarm1.create_offer(swarm2.did()).await.unwrap();
    let answer = swarm2.answer_offer(offer).await.unwrap();
    swarm1.accept_answer(answer).await.unwrap();

    assert!(swarm1.transport.get_connection(swarm2.did()).is_some());
    assert!(swarm2.transport.get_connection(swarm1.did()).is_some());
}
