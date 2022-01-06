use anyhow::Result;
use clap::{App, AppSettings, Arg};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::Duration;
use webrtc::api::APIBuilder;
use webrtc::api::API;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_parameters::DataChannelParameters;
use webrtc::data_channel::RTCDataChannel;
use webrtc::dtls_transport::dtls_parameters::DTLSParameters;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_gatherer::RTCIceGatherOptions;
use webrtc::ice_transport::ice_gatherer::RTCIceGatherer;
use webrtc::ice_transport::ice_parameters::RTCIceParameters;
use webrtc::ice_transport::ice_role::RTCIceRole;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::ice_transport::RTCIceTransport;
use webrtc::peer_connection::math_rand_alpha;
use webrtc::sctp_transport::sctp_transport_capabilities::SCTPTransportCapabilities;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Signal {
    #[serde(rename = "iceCandidates")]
    ice_candidates: Vec<RTCIceCandidate>, // `json:"iceCandidates"`

    #[serde(rename = "iceParameters")]
    ice_parameters: RTCIceParameters, // `json:"iceParameters"`
}

#[derive(Debug, Copy, Clone)]
pub struct Peer {
    api: API,
    gather: Arc<RTCIceGatherer>,
    ice: Arc<RTCIceTransport>,

    // user information
    address: String, // use as label
    is_offering: bool,
}

impl Peer {
    fn new(is_offering: bool, address: String, urls: Vec<String>) -> Result<Self> {
        let mut urls = urls;
        if urls.len() <= 0 {
            urls = vec!["stun:stun.l.google.com:19302".to_owned()];
        }
        let ice_options = RTCIceGatherOptions {
            ice_servers: vec![RTCIceServer {
                urls: urls,
                ..Default::default()
            }],
            ..Default::default()
        };
        api = APIBuilder::new().build();
        gatherer = Arc::new(api.new_ice_gatherer(ice_options)?);
        ice = Arc::new(api.new_ice_transport(Arc::clone(&gatherer)));
        return Peer {
            api,
            gatherer,
            ice,
            address,
            is_offering,
        };
    }
}
