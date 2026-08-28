use super::*;
use crate::core::callback::TransportCallback;

struct NoopCallback;

#[async_trait]
impl TransportCallback for NoopCallback {}

struct RejectingAdmission {
    observed: Arc<std::sync::Mutex<Vec<IpAddr>>>,
}

#[async_trait]
impl UnderlayCandidateAdmission for RejectingAdmission {
    async fn admit(
        &self,
        candidates: &[IpAddr],
    ) -> std::result::Result<(), UnderlayCandidateAdmissionError> {
        *self.observed.lock().expect("candidate observation lock") = candidates.to_vec();
        Err(UnderlayCandidateAdmissionError::new(
            "deliberate test rejection",
        ))
    }
}

fn valid_test_range() -> WebrtcUdpPortRange {
    match WebrtcUdpPortRange::new(49160, 49200) {
        Ok(range) => range,
        Err(error) => panic!("valid range rejected: {error}"),
    }
}

#[test]
fn test_native_udp_range_builds_ephemeral_udp_with_same_bounds() {
    let udp = ephemeral_udp_for_range(valid_test_range());
    let udp = match udp {
        Ok(udp) => udp,
        Err(error) => panic!("valid range rejected by ICE stack: {error}"),
    };

    assert_eq!(udp.port_min(), 49160);
    assert_eq!(udp.port_max(), 49200);
}

#[test]
fn test_native_transport_keeps_configured_udp_range() {
    let range = valid_test_range();
    let transport = WebrtcTransport::new("", None, Some(range));

    assert_eq!(transport.udp_port_range, Some(range));
}

#[test]
fn test_sdp_candidate_ips_are_available_without_nomination() {
    let sdp = "v=0\r\n\
a=candidate:1 1 udp 2130706431 203.0.113.9 49160 typ host\r\n\
a=candidate:2 1 udp 2130706431 2001:db8::9 49161 typ host\r\n\
a=candidate:3 1 udp 2130706431 peer.local 49162 typ host\r\n\
a=candidate:4 1 udp 2130706431 203.0.113.9 49163 typ srflx\r\n";

    assert_eq!(sdp_candidate_ips(sdp), vec![
        "203.0.113.9"
            .parse::<IpAddr>()
            .expect("test IPv4 candidate"),
        "2001:db8::9"
            .parse::<IpAddr>()
            .expect("test IPv6 candidate")
    ]);
}

#[test]
fn test_external_address_candidates_split_trim_and_deduplicate() {
    let candidates = external_address_candidates(Some(" 127.0.0.1, 192.168.215.2,127.0.0.1, "));

    assert_eq!(candidates, vec![
        "127.0.0.1".to_string(),
        "192.168.215.2".to_string()
    ]);
}

#[test]
fn test_external_address_candidates_ignore_blank_config() {
    assert!(external_address_candidates(Some("  ,  ")).is_empty());
    assert!(external_address_candidates(None).is_empty());
}

#[test]
fn test_loopback_external_addresses_are_sdp_only_candidates() {
    let candidates = vec!["127.0.0.1".to_string(), "192.168.215.2".to_string()];

    assert_eq!(nat_1to1_host_candidates(&candidates), vec!["192.168.215.2"]);
    assert_eq!(sdp_extra_host_candidates(&candidates), vec!["127.0.0.1"]);
}

#[test]
fn test_append_sdp_extra_host_candidates_duplicates_host_candidates() {
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

#[tokio::test]
async fn test_rejected_candidate_admission_precedes_remote_description_application() {
    let offerer = WebrtcTransport::new("", None, None);
    let answerer = WebrtcTransport::new("", None, None);
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    answerer
        .set_underlay_candidate_admission(Some(Arc::new(RejectingAdmission {
            observed: Arc::clone(&observed),
        })))
        .await;
    let offer_connection = offerer
        .new_connection("offerer", Box::new(NoopCallback))
        .await
        .expect("offer connection");
    let answer_connection = answerer
        .new_connection("answerer", Box::new(NoopCallback))
        .await
        .expect("answer connection");
    let offer = offer_connection
        .webrtc_create_offer()
        .await
        .expect("local offer");

    let error = answer_connection
        .webrtc_answer_offer(offer)
        .await
        .expect_err("host policy must reject the remote description");

    assert!(matches!(error, Error::UnderlayCandidateAdmission(_)));
    assert!(!observed
        .lock()
        .expect("candidate observation lock")
        .is_empty());
    assert!(answer_connection
        .upgrade()
        .expect("answer connection remains live")
        .webrtc_conn
        .remote_description()
        .await
        .is_none());
    offer_connection.close().await.expect("close offerer");
    answer_connection.close().await.expect("close answerer");
}

#[tokio::test]
async fn test_explicit_underlay_target_uses_installed_admission_policy() {
    let transport = WebrtcTransport::new("", None, None);
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    transport
        .set_underlay_candidate_admission(Some(Arc::new(RejectingAdmission {
            observed: Arc::clone(&observed),
        })))
        .await;
    let targets = vec!["203.0.113.40".parse().expect("test target")];

    let error = transport
        .admit_underlay_targets(&targets)
        .await
        .expect_err("installed policy must gate explicit targets");

    assert!(error.to_string().contains("deliberate test rejection"));
    assert_eq!(
        observed
            .lock()
            .expect("candidate observation lock")
            .as_slice(),
        targets.as_slice()
    );
}
