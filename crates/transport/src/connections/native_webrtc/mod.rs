use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::ice_transport::ice_credential_type::RTCIceCredentialType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::callback::InnerTransportCallback;
use crate::connection_ref::ConnectionRef;
use crate::core::callback::BoxedTransportCallback;
use crate::core::pool::MessageSenderPool;
use crate::core::pool::RoundRobin;
use crate::core::pool::RoundRobinPool;
use crate::core::pool::StatusPool;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::TransportInterface;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::IceCredentialType;
use crate::ice_server::IceServer;
use crate::notifier::Notifier;
use crate::pool::Pool;

const WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT: u8 = 8; // seconds
const WEBRTC_GATHER_TIMEOUT: u8 = 60; // seconds
/// pool size of data channel
const DATA_CHANNEL_POOL_SIZE: u8 = 4;

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl MessageSenderPool<Arc<RTCDataChannel>> for RoundRobinPool<Arc<RTCDataChannel>> {
    type Message = TransportMessage;
    async fn send(&self, msg: TransportMessage) -> Result<()> {
        let channel = self.select()?;
        let data = bincode::serialize(&msg).map(Bytes::from)?;
        if let Err(e) = channel.send(&data).await {
            tracing::error!("{:?}, Data size: {:?}", e, data.len());
            return Err(e.into());
        }
        Ok(())
    }
}

impl StatusPool<Arc<RTCDataChannel>> for RoundRobinPool<Arc<RTCDataChannel>> {
    fn all_ready(&self) -> Result<bool> {
        self.all(|c| c.ready_state() == RTCDataChannelState::Open)
    }
}

/// A connection that implemented by webrtc-rs library.
/// Used for native environment.
pub struct WebrtcConnection {
    webrtc_conn: RTCPeerConnection,
    webrtc_data_channel: Arc<RoundRobinPool<Arc<RTCDataChannel>>>,
    webrtc_data_channel_state_notifier: Notifier,
    cancel_token: CancellationToken,
}

/// [WebrtcTransport] manages all the [WebrtcConnection] and
/// provides methods to create, get and close connections.
pub struct WebrtcTransport {
    ice_servers: Vec<IceServer>,
    external_address: Option<String>,
    pool: Pool<WebrtcConnection>,
}

impl WebrtcConnection {
    fn new(
        webrtc_conn: RTCPeerConnection,
        webrtc_data_channel: Arc<RoundRobinPool<Arc<RTCDataChannel>>>,
        webrtc_data_channel_state_notifier: Notifier,
    ) -> Self {
        Self {
            webrtc_conn,
            webrtc_data_channel,
            webrtc_data_channel_state_notifier,
            cancel_token: CancellationToken::new(),
        }
    }

    async fn webrtc_gather(&self) -> Result<String> {
        let mut gathering_complete_promise = self.webrtc_conn.gathering_complete_promise().await;
        let gathering_complete_promise_with_timeout = tokio::time::timeout(
            std::time::Duration::from_secs(WEBRTC_GATHER_TIMEOUT.into()),
            gathering_complete_promise.recv(),
        );

        tokio::select! {
            _ = self.cancel_token.cancelled() => {
                return Err(Error::WebrtcLocalSdpGenerationError("Local connection closed".to_string()))
            }
            _ = gathering_complete_promise_with_timeout => {}
        }

        Ok(self
            .webrtc_conn
            .local_description()
            .await
            .ok_or(Error::WebrtcLocalSdpGenerationError(
                "Failed to get local description".to_string(),
            ))?
            .sdp)
    }
}

impl WebrtcTransport {
    /// Create a new [WebrtcTransport] instance.
    pub fn new(ice_servers: &str, external_address: Option<String>) -> Self {
        let ice_servers = IceServer::vec_from_str(ice_servers).unwrap();

        Self {
            ice_servers,
            external_address,
            pool: Pool::new(),
        }
    }
}

#[async_trait]
impl ConnectionInterface for WebrtcConnection {
    type Sdp = String;
    type Error = Error;

    async fn send_message(&self, msg: TransportMessage) -> Result<()> {
        self.webrtc_wait_for_data_channel_open().await?;
        self.webrtc_data_channel.send(msg).await
    }

    async fn get_stats(&self) -> Vec<String> {
        self.webrtc_conn
            .get_stats()
            .await
            .reports
            .into_iter()
            .map(|x| serde_json::to_string(&x).unwrap_or("failed to dump stats entry".to_string()))
            .collect()
    }

    fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        self.webrtc_conn.connection_state().into()
    }

    async fn webrtc_create_offer(&self) -> Result<Self::Sdp> {
        let setting_offer = self.webrtc_conn.create_offer(None).await?;
        self.webrtc_conn
            .set_local_description(setting_offer.clone())
            .await?;

        self.webrtc_gather().await
    }

    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp> {
        tracing::debug!("webrtc_answer_offer, offer: {offer:?}");
        let offer = RTCSessionDescription::offer(offer)?;
        self.webrtc_conn.set_remote_description(offer).await?;

        let answer = self.webrtc_conn.create_answer(None).await?;
        self.webrtc_conn
            .set_local_description(answer.clone())
            .await?;

        self.webrtc_gather().await
    }

    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<()> {
        tracing::debug!("webrtc_accept_answer, answer: {answer:?}");
        let answer = RTCSessionDescription::answer(answer)?;
        self.webrtc_conn
            .set_remote_description(answer)
            .await
            .map_err(|e| e.into())
    }

    async fn webrtc_wait_for_data_channel_open(&self) -> Result<()> {
        if matches!(
            self.webrtc_connection_state(),
            WebrtcConnectionState::Failed
                | WebrtcConnectionState::Closed
                | WebrtcConnectionState::Disconnected
        ) {
            return Err(Error::DataChannelOpen("Connection unavailable".to_string()));
        }

        if self.webrtc_data_channel.all_ready()? {
            return Ok(());
        }

        self.webrtc_data_channel_state_notifier
            .set_timeout(WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT);
        self.webrtc_data_channel_state_notifier.clone().await;

        if self.webrtc_data_channel.all_ready()? {
            return Ok(());
        } else {
            return Err(Error::DataChannelOpen(format!(
                "DataChannel not open in {WEBRTC_WAIT_FOR_DATA_CHANNEL_OPEN_TIMEOUT} seconds"
            )));
        }
    }

    async fn close(&self) -> Result<()> {
        self.cancel_token.cancel();
        self.webrtc_conn.close().await.map_err(|e| e.into())
    }
}

#[async_trait]
impl TransportInterface for WebrtcTransport {
    type Connection = WebrtcConnection;
    type Error = Error;

    async fn new_connection(&self, cid: &str, callback: BoxedTransportCallback) -> Result<()> {
        if let Ok(existed_conn) = self.pool.connection(cid) {
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
        // Setup webrtc connection env
        //
        let ice_servers = self.ice_servers.iter().cloned().map(|x| x.into()).collect();

        let webrtc_config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        let mut setting = webrtc::api::setting_engine::SettingEngine::default();
        if let Some(ref addr) = self.external_address {
            tracing::debug!("setting external ip {:?}", addr);
            setting.set_nat_1to1_ips(vec![addr.to_string()], RTCIceCandidateType::Host);
            setting.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        } else {
            setting.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
        }

        let webrtc_api = webrtc::api::APIBuilder::new()
            .with_setting_engine(setting)
            .build();

        //
        // Create webrtc connection
        //
        let webrtc_conn: RTCPeerConnection = webrtc_api.new_peer_connection(webrtc_config).await?;

        //
        // Set callbacks
        //
        let webrtc_data_channel_state_notifier = Notifier::default();
        let inner_cb = Arc::new(InnerTransportCallback::new(
            cid,
            callback,
            webrtc_data_channel_state_notifier.clone(),
        ));

        let channel_pool = Arc::new(RoundRobinPool::default());
        let channel_pool_ref = channel_pool.clone();
        let data_channel_inner_cb = inner_cb.clone();
        webrtc_conn.on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
            let d_label = d.label();
            let d_id = d.id();
            tracing::debug!("New DataChannel {d_label} {d_id}");
            let channel_pool = channel_pool_ref.clone();
            let on_open_inner_cb = data_channel_inner_cb.clone();
            d.on_open(Box::new(move || {
                Box::pin(async move {
                    // check all channels are ready
                    // trigger on_data_channel_open callback iff all channels ready (open)
                    if let Ok(true) = channel_pool.all_ready() {
                        on_open_inner_cb.on_data_channel_open().await
                    }
                })
            }));

            let on_close_inner_cb = data_channel_inner_cb.clone();
            d.on_close(Box::new(move || {
                on_close_inner_cb.on_data_channel_close();
                Box::pin(async move {})
            }));

            let on_message_inner_cb = data_channel_inner_cb.clone();
            d.on_message(Box::new(move |msg: DataChannelMessage| {
                tracing::debug!(
                    "Received DataChannelMessage from {}: {:?}",
                    on_message_inner_cb.cid,
                    msg
                );

                let inner_cb = on_message_inner_cb.clone();

                Box::pin(async move {
                    inner_cb.on_message(&msg.data).await;
                })
            }));

            Box::pin(async move {})
        }));

        let peer_connection_state_change_inner_cb = inner_cb.clone();
        webrtc_conn.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            tracing::debug!("Peer Connection State has changed: {s:?}");

            let inner_cb = peer_connection_state_change_inner_cb.clone();

            Box::pin(async move {
                inner_cb.on_peer_connection_state_change(s.into()).await;
            })
        }));

        //
        // Create data channel
        //
        for i in 0..DATA_CHANNEL_POOL_SIZE {
            let ch = webrtc_conn
                .create_data_channel(&format!("rings_data_channel_{i}"), None)
                .await?;
            channel_pool.push(ch)?;
        }

        //
        // Construct the Connection
        //
        let conn = WebrtcConnection::new(
            webrtc_conn,
            channel_pool,
            webrtc_data_channel_state_notifier,
        );

        self.pool.safely_insert(cid, conn)?;
        Ok(())
    }

    async fn close_connection(&self, cid: &str) -> Result<()> {
        self.pool.safely_remove(cid).await
    }

    fn connection(&self, cid: &str) -> Result<ConnectionRef<Self::Connection>> {
        self.pool.connection(cid)
    }

    fn connections(&self) -> Vec<(String, ConnectionRef<Self::Connection>)> {
        self.pool.connections()
    }

    fn connection_ids(&self) -> Vec<String> {
        self.pool.connection_ids()
    }
}

impl From<IceCredentialType> for RTCIceCredentialType {
    fn from(s: IceCredentialType) -> Self {
        match s {
            IceCredentialType::Password => Self::Password,
            IceCredentialType::Oauth => Self::Oauth,
        }
    }
}

impl From<IceServer> for RTCIceServer {
    fn from(s: IceServer) -> Self {
        Self {
            urls: s.urls,
            username: s.username,
            credential: s.credential,
            credential_type: s.credential_type.into(),
        }
    }
}

impl From<RTCPeerConnectionState> for WebrtcConnectionState {
    fn from(s: RTCPeerConnectionState) -> Self {
        match s {
            RTCPeerConnectionState::Unspecified => Self::Unspecified,
            RTCPeerConnectionState::New => Self::New,
            RTCPeerConnectionState::Connecting => Self::Connecting,
            RTCPeerConnectionState::Connected => Self::Connected,
            RTCPeerConnectionState::Disconnected => Self::Disconnected,
            RTCPeerConnectionState::Failed => Self::Failed,
            RTCPeerConnectionState::Closed => Self::Closed,
        }
    }
}
