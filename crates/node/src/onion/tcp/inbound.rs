use bytes::Bytes;

use crate::onion::OnionExitFailure;

#[derive(Debug)]
pub(super) enum TcpInbound {
    Data(Bytes),
    Shutdown,
    Close,
    Error(OnionExitFailure),
    /// The stream's circuit was destroyed (#963): no frame of it can arrive any more.
    Destroyed,
}
