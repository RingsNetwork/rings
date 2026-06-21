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
use tokio::time::sleep;

use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;
use crate::tests::manually_establish_connection;

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

fn media_channel() -> MediaChannelConfig {
    MediaChannelConfig {
        kind: MediaKind::Video,
        payload_type: 96,
        clock_rate: 90000,
    }
}

fn media_swarm(key: SecretKey, callback: SharedSwarmCallback) -> Arc<Swarm> {
    let stun = "stun://stun.l.google.com:19302";
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    Arc::new(
        SwarmBuilder::new(0, stun, Box::new(MemStorage::new()), session_sk)
            .channel_config(ChannelConfig {
                media: Some(media_channel()),
            })
            .callback(callback)
            .build(),
    )
}

/// End-to-end media with **renegotiation**: two nodes connect data-only, then one attaches a media
/// track *after* the connection is live. `add_media_track` renegotiates over the message layer
/// (RenegotiateSend → RenegotiateReport), the peer learns of the new track via `on_media_track`,
/// and media flows. Real webrtc, drives the full `Swarm` API.
#[tokio::test]
async fn media_track_added_after_connect_via_renegotiation() {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();

    let recorder2 = Arc::new(MediaRecorder::default());
    let received2 = recorder2.received.clone();

    let swarm1 = media_swarm(key1, Arc::new(MediaRecorder::default()));
    let swarm2 = media_swarm(key2, recorder2);
    let did2 = swarm2.did();

    // Connect data-only first (no media track yet).
    manually_establish_connection(&swarm1, &swarm2).await;

    // Attach a media track to the live connection — this triggers renegotiation.
    let local = NativeMediaTrack::new(&media_channel());
    swarm1
        .add_media_track(did2, Box::new(local.clone()))
        .await
        .unwrap();

    // Renegotiation completes asynchronously over the message layer; writes before the track binds
    // are no-ops, so stream until the receiver has seen RTP.
    for _ in 0..200 {
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
        "receiver should have read RTP from the renegotiated media track"
    );
}
