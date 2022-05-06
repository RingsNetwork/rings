use anyhow::{anyhow, Result};
use bytes::Bytes;

use std::sync::Arc;

use webrtc::data_channel::OnCloseHdlrFn;
use webrtc::data_channel::OnMessageHdlrFn;
use webrtc::data_channel::OnOpenHdlrFn;
use webrtc::data_channel::RTCDataChannel;
use webrtc::error::OnErrorHdlrFn;

#[derive(Clone)]
pub struct DataChannel {
    conn: Arc<RTCDataChannel>,
}

impl DataChannel {
    pub fn new(data_channel: RTCDataChannel) -> Self {
        Self {
            conn: Arc::new(data_channel),
        }
    }

    pub async fn on_open(&self, f: OnOpenHdlrFn) -> Result<()> {
        self.conn.on_open(f).await;
        Ok(())
    }

    pub async fn on_close(&self, f: OnCloseHdlrFn) -> Result<()> {
        self.conn.on_close(f).await;
        Ok(())
    }

    pub async fn on_message(&self, f: OnMessageHdlrFn) -> Result<()> {
        self.conn.on_message(f).await;
        Ok(())
    }

    pub async fn on_error(&self, f: OnErrorHdlrFn) -> Result<()> {
        self.conn.on_error(f).await;
        Ok(())
    }

    pub async fn send(&self, data: &Bytes) -> Result<usize> {
        match self.conn.send(data).await {
            Ok(len) => Ok(len),
            Err(e) => Err(anyhow!("WebRTC Send Bytes Failed: {:?}", e)),
        }
    }

    pub async fn send_text(&self, data: String) -> Result<usize> {
        match self.conn.send_text(data).await {
            Ok(len) => Ok(len),
            Err(e) => Err(anyhow!("WebRTC Send String Failed: {:?}", e)),
        }
    }
}
