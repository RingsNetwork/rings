//! The work the swarm runs detached from its caller, named by the error that refuses it.

use std::fmt::Display;
use std::fmt::Formatter;

/// Work the swarm runs detached from its caller and refuses when no runtime is current to
/// carry it (see `swarm::detached`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DetachedWork {
    /// A handshake negotiated in a protocol handler's context.
    ProtocolHandshake,
    /// A link-control send.
    LinkControlSend,
}

impl Display for DetachedWork {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ProtocolHandshake => "a protocol-context handshake",
            Self::LinkControlSend => "a link-control send",
        })
    }
}
