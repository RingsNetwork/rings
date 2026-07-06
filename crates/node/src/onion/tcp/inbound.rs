use bytes::Bytes;

#[derive(Debug)]
pub(super) enum TcpInbound {
    Data(Bytes),
    Shutdown,
    Close,
    Error(String),
}
