use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use lazy_static::lazy_static;
use rand::distributions::Distribution;
use serde::Deserialize;
use serde::Serialize;

use crate::callback::InnerCallback;
use crate::core::callback::BoxedCallback;
use crate::core::transport::SharedConnection;
use crate::core::transport::SharedTransport;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::error::Error;
use crate::error::Result;
use crate::notifier::Notifier;
use crate::Transport;

/// Max delay in ms on sending message
const DUMMY_DELAY_MAX: u64 = 100;
/// Min delay in ms on sending message
const DUMMY_DELAY_MIN: u64 = 0;
/// Config random delay when send message
const SEND_MESSAGE_DELAY: bool = true;
/// Config random delay when channel opening
const CHANNEL_OPEN_DELAY: bool = false;

lazy_static! {
    static ref CBS: DashMap<String, Arc<InnerCallback>> = DashMap::new();
    static ref CONNS: DashMap<String, DummyConnection> = DashMap::new();
}

#[derive(Serialize, Deserialize)]
pub struct DummySdp {
    cid: String,
}

#[derive(Clone)]
pub struct DummyConnection {
    cid: String,
    remote_cid: Arc<Mutex<Option<String>>>,
    webrtc_connection_state: Arc<Mutex<WebrtcConnectionState>>,
}

impl DummyConnection {
    fn new(cid: &str) -> Self {
        Self {
            cid: cid.to_string(),
            remote_cid: Arc::new(Mutex::new(None)),
            webrtc_connection_state: Arc::new(Mutex::new(WebrtcConnectionState::New)),
        }
    }

    fn callback(&self) -> Arc<InnerCallback> {
        CBS.get(&self.cid).unwrap().clone()
    }

    fn remote_callback(&self) -> Arc<InnerCallback> {
        let cid = { self.remote_cid.lock().unwrap().clone() }.unwrap();
        CBS.get(&cid).unwrap().clone()
    }

    fn remote_conn(&self) -> DummyConnection {
        let cid = { self.remote_cid.lock().unwrap().clone() }.unwrap();
        CONNS.get(&cid).unwrap().clone()
    }

    fn set_remote_cid(&self, cid: &str) {
        let mut remote_cid = self.remote_cid.lock().unwrap();
        *remote_cid = Some(cid.to_string());
    }

    async fn set_webrtc_connection_state(&self, state: WebrtcConnectionState) {
        {
            let mut webrtc_connection_state = self.webrtc_connection_state.lock().unwrap();
            *webrtc_connection_state = state;
        }
        self.callback().on_peer_connection_state_change(state).await;
    }
}

#[async_trait]
impl SharedConnection for DummyConnection {
    type Sdp = DummySdp;
    type Error = Error;

    async fn send_message(&self, msg: TransportMessage) -> Result<()> {
        if SEND_MESSAGE_DELAY {
            random_delay().await;
        }
        self.webrtc_wait_for_data_channel_open().await?;
        let data = bincode::serialize(&msg).map(Bytes::from)?;
        self.remote_callback().on_message(&data).await;
        Ok(())
    }

    fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        *self.webrtc_connection_state.lock().unwrap()
    }

    async fn webrtc_create_offer(&self) -> Result<Self::Sdp> {
        self.set_webrtc_connection_state(WebrtcConnectionState::Connecting)
            .await;
        Ok(DummySdp {
            cid: self.cid.clone(),
        })
    }

    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp> {
        self.set_webrtc_connection_state(WebrtcConnectionState::Connecting)
            .await;
        self.set_remote_cid(&offer.cid);
        Ok(DummySdp {
            cid: self.cid.clone(),
        })
    }

    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<()> {
        self.set_webrtc_connection_state(WebrtcConnectionState::Connected)
            .await;
        self.set_remote_cid(&answer.cid);
        self.remote_conn()
            .set_webrtc_connection_state(WebrtcConnectionState::Connected)
            .await;
        Ok(())
    }

    async fn webrtc_wait_for_data_channel_open(&self) -> Result<()> {
        if CHANNEL_OPEN_DELAY {
            random_delay().await;
        }
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        self.set_webrtc_connection_state(WebrtcConnectionState::Closed)
            .await;
        self.remote_conn()
            .set_webrtc_connection_state(WebrtcConnectionState::Closed)
            .await;
        Ok(())
    }
}

#[async_trait]
impl SharedTransport for Transport<DummyConnection> {
    type Connection = DummyConnection;
    type Error = Error;

    async fn new_connection(
        &self,
        cid: &str,
        callback: Arc<BoxedCallback>,
    ) -> Result<Self::Connection> {
        if let Ok(existed_conn) = self.get_connection(cid) {
            if matches!(
                existed_conn.webrtc_connection_state(),
                WebrtcConnectionState::New
                    | WebrtcConnectionState::Connecting
                    | WebrtcConnectionState::Connected
            ) {
                return Err(Error::ConnectionAlreadyExists(cid.to_string()));
            }
        }

        //
        // Construct the Connection
        //
        let conn = DummyConnection::new(cid);

        self.safely_insert(cid, conn.clone())?;
        CONNS.insert(cid.to_string(), conn.clone());
        CBS.insert(
            cid.to_string(),
            Arc::new(InnerCallback::new(cid, callback, Notifier::default())),
        );
        Ok(conn)
    }
}

async fn random_delay() {
    tokio::time::sleep(Duration::from_millis(random())).await;
}

fn random() -> u64 {
    let range = rand::distributions::Uniform::new(DUMMY_DELAY_MIN, DUMMY_DELAY_MAX);
    let mut rng = rand::thread_rng();
    range.sample(&mut rng)
}
