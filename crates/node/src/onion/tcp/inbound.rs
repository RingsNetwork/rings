use bytes::Bytes;

use crate::onion::OnionExitFailure;

#[derive(Debug)]
pub(super) enum TcpInbound {
    Data(Bytes),
    Shutdown,
    Close,
    Error(OnionExitFailure),
}
