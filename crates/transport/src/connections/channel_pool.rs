//! implementation of datachannel pool in for rings transport
//! ===============

#[cfg(feature="native-webrtc")]
use webrtc::data_channel::RTCDataChannel;
#[cfg(feature="native-webrtc")]
use webrtc::peer_connection::RTCPeerConnection;

#[cfg(feature="web-sys-webrtc")]
use web_sys::RtcDataChannel as RTCDataChannel;
#[cfg(feature="web-sys-webrtc")]
use web_sys::RtcPeerConnection as RTCPeerConnection;

use std::sync::Arc;

use crate::error::Result;

pub trait RoundRobinPool {
    type Target;
    fn select(&self) -> Self::Target;
}

pub trait DataChannelPool: RoundRobinPool {
    fn send(&self, msg: String) -> Result<()>;
    fn on_data_channel(&self, callback: Box<fn(Self::Target) -> Result<()>>);
}

/// Pool for webRTC dataChannel
pub struct RTCDataChannelPool {
    pool: Vec<Arc<RTCDataChannel>>,
    // todo: be atomic
    idx: std::sync::atomic::AtomicU8
}

impl RTCDataChannelPool {
    /// create a new data channel with given size
    pub async fn new(conn: &RTCPeerConnection, size: usize) -> Result<Self> {
	let mut pool = vec![];
	for i in 0..size {
	    #[cfg(feature="native-webrtc")]
	    let channel = conn.create_data_channel(&format!("rings-{}", i), None).await?;
	    #[cfg(feature="web-sys-webrtc")]
	    let channel = conn.create_data_channel(&format!("rings-{}", i));

	    pool.push(channel)
	}
	Ok(Self {
	    pool,
	    idx: 0.into()
	})
    }
}

impl RoundRobinPool for RTCDataChannelPool {
    type Target = Arc<RTCDataChannel>;
    fn select(&self) -> Self::Target {
	let ret = self.pool[self.idx.into_inner() as usize].clone();
	let mut idx = self.idx.get_mut();
	*idx = (*idx + 1) % self.pool.len() as u8;
	ret
    }
}



impl DataChannelPool for RTCDataChannelPool {
    fn send(&self, msg: String) -> Result<()> {
	unimplemented!()
    }

    fn on_data_channel(&self, callback: Box<fn(Self::Target) -> Result<()>>) {
	unimplemented!()
    }
}
