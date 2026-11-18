use std::str::FromStr;

use super::*;

#[test]
fn test_parsing() {
    let a = "stun://foo:bar@stun.l.google.com:19302";
    let b = "turn://ethereum.org:9090";
    let c = "turn://ryan@ethereum.org:9090/nginx/v2";
    let d = "turn://ryan@ethereum.org/nginx/v2";
    let e = "http://ryan@ethereum.org/nginx/v2";
    let ret_a = IceServer::from_str(a).unwrap();
    let ret_b = IceServer::from_str(b).unwrap();
    let ret_c = IceServer::from_str(c).unwrap();
    let ret_d = IceServer::from_str(d).unwrap();
    let ret_e = IceServer::from_str(e);

    assert_eq!(ret_a.urls[0], "stun:stun.l.google.com:19302".to_string());
    assert_eq!(ret_a.credential, "bar".to_string());
    assert_eq!(ret_a.username, "foo".to_string());

    assert_eq!(ret_b.urls[0], "turn:ethereum.org:9090".to_string());
    assert_eq!(ret_b.credential, "".to_string());
    assert_eq!(ret_b.username, "".to_string());

    assert_eq!(ret_c.urls[0], "turn:ethereum.org:9090/nginx/v2".to_string());
    assert_eq!(ret_c.credential, "".to_string());
    assert_eq!(ret_c.username, "ryan".to_string());

    assert_eq!(ret_d.urls[0], "turn:ethereum.org/nginx/v2".to_string());
    assert_eq!(ret_d.credential, "".to_string());
    assert_eq!(ret_d.username, "ryan".to_string());

    assert!(ret_e.is_err());

    let tcp_turn =
        IceServer::from_str("turn://user:pass@turn.example.com:80?transport=tcp").unwrap();
    assert_eq!(
        tcp_turn.urls[0],
        "turn:turn.example.com:80?transport=tcp".to_string()
    );
    assert_eq!(tcp_turn.username, "user".to_string());
    assert_eq!(tcp_turn.credential, "pass".to_string());

    let turns =
        IceServer::from_str("turns://user:pass@turn.example.com:5349?transport=tcp").unwrap();
    assert_eq!(
        turns.urls[0],
        "turns:turn.example.com:5349?transport=tcp".to_string()
    );
    assert_eq!(turns.username, "user".to_string());
    assert_eq!(turns.credential, "pass".to_string());

    let stuns = IceServer::from_str("stuns://stun.l.google.com:5349").unwrap();
    assert_eq!(stuns.urls[0], "stuns:stun.l.google.com:5349".to_string());

    let rfc7065 = IceServer::from_str("turn:turn.example.com:3478?transport=tcp").unwrap();
    assert_eq!(
        rfc7065.urls[0],
        "turn:turn.example.com:3478?transport=tcp".to_string()
    );
}

#[test]
fn test_parsing_rejects_missing_host() {
    let parsed = IceServer::from_str("stun:///missing-host");
    assert!(parsed.is_err());
}

#[cfg(any(
    all(feature = "dummy", not(target_family = "wasm")),
    all(feature = "native-webrtc", not(target_family = "wasm")),
    all(feature = "web-sys-webrtc", target_family = "wasm"),
))]
#[test]
fn test_blank_ice_server_config_means_no_servers() {
    assert!(parse_ice_servers_or_warn("", "test").is_empty());
    assert!(parse_ice_servers_or_warn("   ", "test").is_empty());
}

/// Unsupported OAuth configuration is rejected instead of silently using passwords.
#[test]
fn test_oauth_credential_is_rejected() {
    assert!(serde_json::from_str::<IceCredentialType>(r#""Oauth""#).is_err());
    assert_eq!(
        serde_json::from_str::<IceCredentialType>(r#""Password""#).unwrap(),
        IceCredentialType::Password,
    );
}
