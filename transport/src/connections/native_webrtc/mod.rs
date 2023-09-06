use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::mapref::entry::Entry;
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

use crate::callback::InnerCallback;
use crate::core::callback::BoxedCallback;
use crate::core::transport::SharedConnection;
use crate::core::transport::SharedTransport;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::IceCredentialType;
use crate::ice_server::IceServer;
use crate::Transport;

#[derive(Clone)]
pub struct WebrtcConnection {
    webrtc_conn: Arc<RTCPeerConnection>,
    webrtc_data_channel: Arc<RTCDataChannel>,
}

impl WebrtcConnection {
    pub fn new(webrtc_conn: RTCPeerConnection, webrtc_data_channel: Arc<RTCDataChannel>) -> Self {
        Self {
            webrtc_conn: Arc::new(webrtc_conn),
            webrtc_data_channel,
        }
    }

    async fn webrtc_gather(&self) -> Result<RTCSessionDescription> {
        self.webrtc_conn
            .gathering_complete_promise()
            .await
            .recv()
            .await;

        self.webrtc_conn
            .local_description()
            .await
            .ok_or(Error::WebrtcLocalSdpGenerationError)
    }

    async fn close(&self) -> Result<()> {
        self.webrtc_conn.close().await.map_err(|e| e.into())
    }
}

#[async_trait]
impl SharedConnection for WebrtcConnection {
    type Sdp = RTCSessionDescription;
    type Error = Error;

    async fn send_message(&self, msg: TransportMessage) -> Result<()> {
        self.webrtc_wait_for_data_channel_open().await?;
        let data = bincode::serialize(&msg).map(Bytes::from)?;
        self.webrtc_data_channel.send(&data).await?;
        Ok(())
    }

    fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        self.webrtc_conn.connection_state().into()
    }

    async fn webrtc_create_offer(&self) -> Result<RTCSessionDescription> {
        let setting_offer = self.webrtc_conn.create_offer(None).await?;
        self.webrtc_conn
            .set_local_description(setting_offer.clone())
            .await?;

        self.webrtc_gather().await
    }

    async fn webrtc_answer_offer(
        &self,
        offer: RTCSessionDescription,
    ) -> Result<RTCSessionDescription> {
        tracing::debug!("webrtc_answer_offer, offer: {offer:?}");

        self.webrtc_conn.set_remote_description(offer).await?;

        let answer = self.webrtc_conn.create_answer(None).await?;
        self.webrtc_conn
            .set_local_description(answer.clone())
            .await?;

        self.webrtc_gather().await
    }

    async fn webrtc_accept_answer(&self, answer: RTCSessionDescription) -> Result<()> {
        tracing::debug!("webrtc_accept_answer, answer: {answer:?}");

        self.webrtc_conn
            .set_remote_description(answer)
            .await
            .map_err(|e| e.into())
    }

    async fn webrtc_wait_for_data_channel_open(&self) -> Result<()> {
        loop {
            if matches!(
                self.webrtc_connection_state(),
                WebrtcConnectionState::Failed
                    | WebrtcConnectionState::Closed
                    | WebrtcConnectionState::Disconnected
            ) {
                return Err(Error::DataChannelOpen("Connection unavailable".to_string()));
            }

            if self.webrtc_data_channel.ready_state() == RTCDataChannelState::Open {
                return Ok(());
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[async_trait]
impl SharedTransport for Transport<WebrtcConnection> {
    type Connection = WebrtcConnection;
    type Error = Error;

    async fn new_connection<CE>(
        &self,
        cid: &str,
        callback: Arc<BoxedCallback<CE>>,
    ) -> Result<Self::Connection>
    where
        CE: std::error::Error + Send + Sync + 'static,
    {
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
            setting.set_ice_multicast_dns_mode(MulticastDnsMode::QueryOnly);
        } else {
            // mDNS gathering cannot be used with 1:1 NAT IP mapping for host candidate
            setting.set_ice_multicast_dns_mode(MulticastDnsMode::QueryAndGather);
        }

        let webrtc_api = webrtc::api::APIBuilder::new()
            .with_setting_engine(setting)
            .build();

        //
        // Create webrtc connection
        //
        let webrtc_conn = webrtc_api.new_peer_connection(webrtc_config).await?;

        //
        // Set callbacks
        //
        let inner_cb = Arc::new(InnerCallback::new(cid, callback));

        let data_channel_inner_cb = inner_cb.clone();
        webrtc_conn.on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
            let d_label = d.label();
            let d_id = d.id();
            tracing::debug!("New DataChannel {d_label} {d_id}");

            let inner_cb = data_channel_inner_cb.clone();

            Box::pin(async move {
                d.on_message(Box::new(move |msg: DataChannelMessage| {
                    tracing::debug!(
                        "Received DataChannelMessage from {}: {:?}",
                        inner_cb.cid,
                        msg
                    );

                    let inner_cb = inner_cb.clone();

                    Box::pin(async move {
                        inner_cb.on_message(&msg.data).await;
                    })
                }));
            })
        }));

        let peer_connection_state_change_inner_cb = inner_cb.clone();
        webrtc_conn.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            tracing::debug!("Peer Connection State has changed: {s}");

            let inner_cb = peer_connection_state_change_inner_cb.clone();

            Box::pin(async move {
                inner_cb.on_peer_connection_state_change(s.into()).await;
            })
        }));

        //
        // Create data channel
        //
        let webrtc_data_channel = webrtc_conn.create_data_channel("rings", None).await?;

        //
        // Construct the Connection
        //
        let conn = WebrtcConnection::new(webrtc_conn, webrtc_data_channel);

        //
        // Safely insert
        //
        // The implementation of match statement refers to Entry::insert in dashmap.
        // An extra check is added to see if the connection is already connected.
        // See also: https://docs.rs/dashmap/latest/dashmap/mapref/entry/enum.Entry.html#method.insert
        //
        let Some(entry) = self.connections.try_entry(cid.to_string()) else {
            return Err(Error::ConnectionAlreadyExists(cid.to_string()));
        };
        match entry {
            Entry::Occupied(mut entry) => {
                let existed_conn = entry.get();
                if matches!(
                    existed_conn.webrtc_connection_state(),
                    WebrtcConnectionState::New
                        | WebrtcConnectionState::Connecting
                        | WebrtcConnectionState::Connected
                ) {
                    return Err(Error::ConnectionAlreadyExists(cid.to_string()));
                }

                entry.insert(conn.clone());
                entry.into_ref()
            }
            Entry::Vacant(entry) => entry.insert(conn.clone()),
        };

        Ok(conn)
    }

    fn get_connection(&self, cid: &str) -> Result<Self::Connection> {
        self.connections
            .get(cid)
            .map(|c| c.value().clone())
            .ok_or(Error::ConnectionNotFound(cid.to_string()))
    }

    fn get_connections(&self) -> Vec<(String, Self::Connection)> {
        self.connections
            .iter()
            .map(|kv| (kv.key().clone(), kv.value().clone()))
            .collect()
    }

    fn get_connection_ids(&self) -> Vec<String> {
        self.connections.iter().map(|kv| kv.key().clone()).collect()
    }

    async fn close_connection(&self, cid: &str) -> Result<()> {
        let conn = self.get_connection(cid)?;
        conn.close().await?;
        self.connections.remove(cid);
        Ok(())
    }
}

impl From<IceCredentialType> for RTCIceCredentialType {
    fn from(s: IceCredentialType) -> Self {
        match s {
            IceCredentialType::Unspecified => Self::Unspecified,
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
