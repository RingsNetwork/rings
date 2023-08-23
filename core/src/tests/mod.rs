use rings_transport::core::transport::SharedConnection;

use crate::swarm::Swarm;

#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(all(not(feature = "wasm")))]
pub mod default;

#[allow(dead_code)]
pub fn setup_tracing() {
    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(tracing::Level::DEBUG)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
}

pub async fn manually_establish_connection(swarm1: &Swarm, swarm2: &Swarm) {
    assert!(swarm1.get_connection(swarm2.did()).is_none());
    assert!(swarm2.get_connection(swarm1.did()).is_none());

    let conn1 = swarm1.new_connection(swarm2.did()).await.unwrap();
    let conn2 = swarm2.new_connection(swarm1.did()).await.unwrap();

    let offer = conn1.webrtc_create_offer().await.unwrap();
    let answer = conn2.webrtc_answer_offer(offer).await.unwrap();
    conn1.webrtc_accept_answer(answer).await.unwrap();

    assert!(swarm1.get_connection(swarm2.did()).is_some());
    assert!(swarm2.get_connection(swarm1.did()).is_some());
}
