use std::io::ErrorKind as IOErrorKind;

use serde::Deserialize;
use serde::Serialize;

#[derive(Deserialize, Serialize, Debug, Clone, Copy)]
#[repr(u8)]
pub enum TunnelDefeat {
    None = 0,
    WebrtcDatachannelSendFailed = 1,
    ConnectionTimeout = 2,
    ConnectionRefused = 3,
    ConnectionAborted = 4,
    ConnectionReset = 5,
    NotConnected = 6,
    ConnectionClosed = 7,
    Unknown = u8::MAX,
}

impl From<IOErrorKind> for TunnelDefeat {
    fn from(kind: IOErrorKind) -> TunnelDefeat {
        match kind {
            IOErrorKind::ConnectionRefused => TunnelDefeat::ConnectionRefused,
            IOErrorKind::ConnectionAborted => TunnelDefeat::ConnectionAborted,
            IOErrorKind::ConnectionReset => TunnelDefeat::ConnectionReset,
            IOErrorKind::NotConnected => TunnelDefeat::NotConnected,
            _ => TunnelDefeat::Unknown,
        }
    }
}
