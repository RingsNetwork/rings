use super::*;

fn valid_test_range() -> WebrtcUdpPortRange {
    match WebrtcUdpPortRange::new(49160, 49200) {
        Ok(range) => range,
        Err(error) => panic!("valid range rejected: {error}"),
    }
}

#[test]
fn native_udp_range_builds_ephemeral_udp_with_same_bounds() {
    let udp = ephemeral_udp_for_range(valid_test_range());
    let udp = match udp {
        Ok(udp) => udp,
        Err(error) => panic!("valid range rejected by ICE stack: {error}"),
    };

    assert_eq!(udp.port_min(), 49160);
    assert_eq!(udp.port_max(), 49200);
}

#[test]
fn native_transport_keeps_configured_udp_range() {
    let range = valid_test_range();
    let transport = WebrtcTransport::new("", None, Some(range));

    assert_eq!(transport.udp_port_range, Some(range));
}

#[test]
fn external_address_candidates_split_trim_and_deduplicate() {
    let candidates = external_address_candidates(Some(" 127.0.0.1, 192.168.215.2,127.0.0.1, "));

    assert_eq!(candidates, vec![
        "127.0.0.1".to_string(),
        "192.168.215.2".to_string()
    ]);
}

#[test]
fn external_address_candidates_ignore_blank_config() {
    assert!(external_address_candidates(Some("  ,  ")).is_empty());
    assert!(external_address_candidates(None).is_empty());
}

#[test]
fn loopback_external_addresses_are_sdp_only_candidates() {
    let candidates = vec!["127.0.0.1".to_string(), "192.168.215.2".to_string()];

    assert_eq!(nat_1to1_host_candidates(&candidates), vec!["192.168.215.2"]);
    assert_eq!(sdp_extra_host_candidates(&candidates), vec!["127.0.0.1"]);
}

#[test]
fn append_sdp_extra_host_candidates_duplicates_host_candidates() {
    let sdp = "v=0\r\n\
a=candidate:1 1 udp 2130706431 192.168.215.2 49160 typ host\r\n\
a=end-of-candidates\r\n"
        .to_string();

    let rewritten = append_sdp_extra_host_candidates(sdp, &["127.0.0.1".to_string()]);

    assert!(rewritten.contains(
        "a=candidate:1 1 udp 2130706431 192.168.215.2 49160 typ host\r\n\
a=candidate:1 1 udp 2130706431 127.0.0.1 49160 typ host\r\n"
    ));
    assert!(rewritten.ends_with("a=end-of-candidates\r\n"));
}
