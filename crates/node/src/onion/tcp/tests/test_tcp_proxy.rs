use super::super::duplex::TcpDuplexState;

/// Law (half-close): each half closes independently and stays closed; the stream is over
/// exactly when both are.
#[test]
fn test_tcp_duplex_state_closes_only_after_both_halves_close() {
    let mut state = TcpDuplexState::open();
    assert!(state.can_read() && state.can_write() && !state.is_closed());

    state.close_read();
    assert!(!state.can_read());
    assert!(state.can_write());
    assert!(!state.is_closed());

    state.close_read();
    assert!(!state.can_read());

    state.close_write();
    assert!(state.is_closed());
}

/// Law (commutation): the closed state does not depend on the order the halves close in.
#[test]
fn test_tcp_duplex_state_close_order_commutes() {
    let mut read_first = TcpDuplexState::open();
    read_first.close_read();
    read_first.close_write();
    let mut write_first = TcpDuplexState::open();
    write_first.close_write();
    write_first.close_read();

    assert_eq!(read_first, write_first);
}
