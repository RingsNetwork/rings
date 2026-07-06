/// TCP duplex close state for one onion stream.
///
/// Invariant: `remote_terminal_seen => !read_open && !write_open`.
/// Preservation: local half-closes only clear one half and still announce a terminal frame when
/// both halves close; observing a remote terminal clears both halves and suppresses echo.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TcpDuplexState {
    read_open: bool,
    write_open: bool,
    remote_terminal_seen: bool,
}

impl TcpDuplexState {
    pub(super) const fn open() -> Self {
        Self {
            read_open: true,
            write_open: true,
            remote_terminal_seen: false,
        }
    }

    pub(super) const fn can_read(self) -> bool {
        self.read_open
    }

    pub(super) const fn can_write(self) -> bool {
        self.write_open
    }

    pub(super) const fn is_closed(self) -> bool {
        !self.read_open && !self.write_open
    }

    pub(super) const fn should_announce_terminal(self) -> bool {
        !self.remote_terminal_seen
    }

    pub(super) fn close_read(&mut self) {
        self.read_open = false;
    }

    pub(super) fn close_write(&mut self) {
        self.write_open = false;
    }

    pub(super) fn observe_remote_terminal(&mut self) {
        self.read_open = false;
        self.write_open = false;
        self.remote_terminal_seen = true;
    }
}
