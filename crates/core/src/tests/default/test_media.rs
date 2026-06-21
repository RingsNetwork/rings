use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use rings_transport::connections::NativeMediaTrack;
use rings_transport::connections::NativeRemoteTrack;
use rings_transport::core::media::ChannelConfig;
use rings_transport::core::media::MediaChannelConfig;
use rings_transport::core::media::MediaKind;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::WebrtcConnectionState;
use tokio::time::sleep;

use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;

/// Counts RTP packets arriving on remote media tracks delivered to `on_media_track`.
#[derive(Default)]
struct MediaRecorder {
    received: Arc<AtomicUsize>,
}

#[async_trait]
impl SwarmCallback for MediaRecorder {
    async fn on_media_track(
        &self,
        _peer: Did,
        track: rings_transport::core::media::BoxedMediaTrack,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Consume the remote track per platform: here (native) drain its RTP and count packets.
        if let Some(remote) = track.as_any().downcast_ref::<NativeRemoteTrack>() {
            let remote = remote.clone();
            let received = self.received.clone();
            tokio::spawn(async move {
                while remote.read_rtp().await.is_ok() {
                    received.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
        Ok(())
    }
}

fn media_config() -> ChannelConfig {
    ChannelConfig {
        media: Some(MediaChannelConfig {
            kind: MediaKind::Video,
            payload_type: 96,
            clock_rate: 90000,
        }),
    }
}

fn media_swarm(key: SecretKey) -> Arc<Swarm> {
    let stun = "stun://stun.l.google.com:19302";
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    Arc::new(
        SwarmBuilder::new(0, stun, Box::new(MemStorage::new()), session_sk)
            .channel_config(media_config())
            .build(),
    )
}

async fn wait_connected(conn: &impl ConnectionInterface) {
    for _ in 0..200 {
        if conn.webrtc_connection_state() == WebrtcConnectionState::Connected {
            return;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("connection did not reach Connected");
}

/// Real native media: two media-enabled nodes negotiate, one attaches a local track and writes
/// samples, the other receives the track via `on_media_track` and reads its RTP — proving the
/// platform-consistent track API carries media end to end (real webrtc).
#[tokio::test]
async fn media_track_flows_over_native() {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let swarm1 = media_swarm(key1);
    let swarm2 = media_swarm(key2);
    let (did1, did2) = (swarm1.did(), swarm2.did());

    let recorder2 = Arc::new(MediaRecorder::default());
    let received2 = recorder2.received.clone();

    // Wire each connection's callback (sender's is a throwaway recorder).
    let cb1 = InnerSwarmCallback::new(swarm1.transport.clone(), Arc::new(MediaRecorder::default()));
    let cb2 = InnerSwarmCallback::new(swarm2.transport.clone(), recorder2);
    swarm1.transport.new_connection(did2, cb1).await.unwrap();
    swarm2.transport.new_connection(did1, cb2).await.unwrap();

    let conn1 = swarm1.transport.get_connection(did2).unwrap();
    let conn2 = swarm2.transport.get_connection(did1).unwrap();

    // Attach the local media track *before* the offer so the m= section is negotiated; keep a clone
    // to write samples into the same underlying track.
    let local = NativeMediaTrack::new(&MediaChannelConfig {
        kind: MediaKind::Video,
        payload_type: 96,
        clock_rate: 90000,
    });
    conn1
        .add_media_track(Box::new(local.clone()))
        .await
        .unwrap();

    // Manual offer / answer / accept (the raw SDP exchange swarm.create_offer wraps).
    let offer = conn1.connection.webrtc_create_offer().await.unwrap();
    let answer = conn2.connection.webrtc_answer_offer(offer).await.unwrap();
    conn1.connection.webrtc_accept_answer(answer).await.unwrap();

    wait_connected(&conn1.connection).await;

    // Stream samples until the receiver has seen RTP (in-process loopback is reliable once up).
    for _ in 0..100 {
        local
            .write_sample(
                Bytes::from_static(b"a media frame payload"),
                Duration::from_millis(20),
            )
            .await
            .unwrap();
        if received2.load(Ordering::SeqCst) > 0 {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }

    assert!(
        received2.load(Ordering::SeqCst) > 0,
        "receiver should have read RTP from the remote media track"
    );
}
