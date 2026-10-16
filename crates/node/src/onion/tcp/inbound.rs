use std::sync::Arc;
use std::sync::OnceLock;

use bytes::Bytes;
use tokio::sync::mpsc;

use super::pump::TcpStreamAbort;
use crate::onion::OnionExitFailure;

#[derive(Debug)]
pub(super) enum TcpInbound {
    Data(Bytes),
    Shutdown,
    Close,
    Error(OnionExitFailure),
}

/// The cause a stream's circuit was ended with, recorded beside its inbound lane (#963, #966).
///
/// Law: a teardown records the cause and then releases the stream's record, whose sender is the
/// lane's last, so the pump reads the cause once the lane closes, behind every frame already
/// queued. Releasing the record never waits for room, so a full lane cannot lose the cause. The
/// first cause recorded stands.
#[derive(Clone, Debug, Default)]
pub(super) struct TcpStreamFate(Arc<OnceLock<TcpStreamAbort>>);

impl TcpStreamFate {
    /// Record `cause`, unless a cause is recorded already.
    pub(super) fn end(&self, cause: TcpStreamAbort) {
        // A later cause loses to the first: the stream ends once.
        let _ = self.0.set(cause);
    }

    /// The cause of a stream whose lane closed: the recorded one, or `RemoteClosed` when its record
    /// was released without one.
    pub(super) fn closed(&self) -> TcpStreamAbort {
        self.0
            .get()
            .cloned()
            .unwrap_or(TcpStreamAbort::RemoteClosed)
    }
}

/// The pump's side of one stream's inbound lane: its bounded frames, and its fate beside them.
#[derive(Debug)]
pub(super) struct TcpInboundLane {
    pub(super) frames: mpsc::Receiver<TcpInbound>,
    pub(super) fate: TcpStreamFate,
}

/// The record's side of one stream's inbound lane.
#[derive(Debug)]
pub(super) struct TcpInboundSender {
    pub(super) frames: mpsc::Sender<TcpInbound>,
    pub(super) fate: TcpStreamFate,
}

/// A new inbound lane of `capacity` frames.
pub(super) fn tcp_inbound_lane(capacity: usize) -> (TcpInboundSender, TcpInboundLane) {
    let (sender, receiver) = mpsc::channel(capacity);
    let fate = TcpStreamFate::default();
    (
        TcpInboundSender {
            frames: sender,
            fate: fate.clone(),
        },
        TcpInboundLane {
            frames: receiver,
            fate,
        },
    )
}
