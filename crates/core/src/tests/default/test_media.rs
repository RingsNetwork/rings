use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::lock::Mutex;
use rings_transport::core::media::ChannelConfig;
use rings_transport::core::media::MediaChannelConfig;
use rings_transport::core::media::MediaKind;
use rings_transport::core::media::RtpPacket;
use rings_transport::core::transport::WebrtcConnectionState;
use tokio::time::sleep;
use tokio::time::Duration;

use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::media::MediaFrame;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;
use crate::tests::manually_establish_connection;

/// Records the media frames delivered to a node via `on_media_frame`.
#[derive(Default)]
struct MediaRecorder {
    frames: Mutex<Vec<MediaFrame>>,
}

#[async_trait]
impl SwarmCallback for MediaRecorder {
    async fn on_media_frame(
        &self,
        _peer: Did,
        frame: MediaFrame,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.frames.lock().await.push(frame);
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

async fn media_swarm(key: SecretKey, recorder: Arc<MediaRecorder>) -> Arc<Swarm> {
    let stun = "stun://stun.l.google.com:19302";
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let swarm = SwarmBuilder::new(0, stun, Box::new(MemStorage::new()), session_sk)
        .channel_config(media_config())
        .callback(recorder)
        .build();
    Arc::new(swarm)
}

/// Poll until the connection to `peer` reaches `Connected`, or panic after a timeout.
async fn wait_connected(swarm: &Swarm, peer: Did) {
    for _ in 0..200 {
        if let Some(conn) = swarm.transport.get_connection(peer) {
            if conn.webrtc_connection_state() == WebrtcConnectionState::Connected {
                return;
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("connection to {peer} did not reach Connected");
}

/// Real native RTP over webrtc: two media-enabled nodes establish a connection, one streams RTP
/// packets, and the other receives them reassembled into ordered media frames via `on_media_frame`.
#[tokio::test]
async fn media_frames_flow_over_native_rtp() {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let recorder2 = Arc::new(MediaRecorder::default());

    let swarm1 = media_swarm(key1, Arc::new(MediaRecorder::default())).await;
    let swarm2 = media_swarm(key2, recorder2.clone()).await;

    manually_establish_connection(&swarm1, &swarm2).await;
    wait_connected(&swarm1, swarm2.did()).await;
    // Let DTLS/SRTP settle before pushing media.
    sleep(Duration::from_millis(500)).await;

    // Five single-packet frames (marker set), one per increasing timestamp.
    let expected: Vec<u32> = vec![100, 200, 300, 400, 500];

    // RTP is lossy and the first packets can race SRTP setup, so resend the whole sequence until
    // all frames are received. A resent frame whose timestamp was already emitted is dropped as
    // "late" by the depacketizer, so duplicates can't corrupt the result.
    for _ in 0..50 {
        for (i, ts) in expected.iter().enumerate() {
            let packet = RtpPacket {
                sequence: i as u16,
                timestamp: *ts,
                marker: true,
                payload: Bytes::from(vec![i as u8; 8]),
            };
            swarm1.send_media(swarm2.did(), packet).await.unwrap();
        }
        if recorder2.frames.lock().await.len() >= expected.len() {
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }

    let frames = recorder2.frames.lock().await;
    let timestamps: Vec<u32> = frames.iter().map(|f| f.timestamp).collect();
    assert_eq!(
        timestamps, expected,
        "received media frames in timestamp order"
    );
    assert_eq!(frames[0].data, Bytes::from(vec![0u8; 8]));
}
