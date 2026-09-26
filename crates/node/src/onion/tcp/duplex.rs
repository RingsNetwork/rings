/// The half-close state of one relayed byte stream: its local read and write halves.
///
/// Invariant: a half, once closed, stays closed; the stream is over when both are.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TcpDuplexState {
    /// Whether the local stream may still be read (no local EOF yet).
    read_open: bool,
    /// Whether the local stream may still be written (no remote `fin` yet).
    write_open: bool,
}

impl TcpDuplexState {
    /// Both halves open.
    pub(super) const fn open() -> Self {
        Self {
            read_open: true,
            write_open: true,
        }
    }

    /// Whether the local stream may still be read.
    pub(super) const fn can_read(self) -> bool {
        self.read_open
    }

    /// Whether the local stream may still be written.
    pub(super) const fn can_write(self) -> bool {
        self.write_open
    }

    /// Whether both halves are closed.
    pub(super) const fn is_closed(self) -> bool {
        !self.read_open && !self.write_open
    }

    /// The local stream ended: its read half closes.
    pub(super) fn close_read(&mut self) {
        self.read_open = false;
    }

    /// The remote stream ended: the local write half closes.
    pub(super) fn close_write(&mut self) {
        self.write_open = false;
    }
}
