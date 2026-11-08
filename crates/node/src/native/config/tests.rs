use rings_core::dht::Did;

use super::*;
use crate::native::bootstrap::BootstrapTargets;

fn dumped_delegatee_key() -> String {
    let key = SecretKey::random();
    let session = match DelegateeKey::new_with_seckey(&key) {
        Ok(session) => session,
        Err(error) => panic!("delegatee key construction failed: {error}"),
    };
    match session.dump() {
        Ok(dump) => dump,
        Err(error) => panic!("delegatee key dump failed: {error}"),
    }
}

/// Write a valid delegatee key to an isolated file and return its config plus cleanup path.
fn config_with_delegatee_key_file() -> (Config, PathBuf) {
    let path =
        std::env::temp_dir().join(format!("rings-delegatee-key-{}.yaml", uuid::Uuid::new_v4()));
    fs::write(&path, dumped_delegatee_key()).expect("write test delegatee key");
    (Config::new(&path), path)
}

#[test]
fn test_deserialization_defaults_online_registration_fields() {
    let yaml = r#"
network_id: 1
delegatee_key: delegatee_key
internal_api_port: 50000
external_api_addr: 127.0.0.1:50001
endpoint_url: http://127.0.0.1:50000
ice_servers: stun://stun.l.google.com:19302
stabilize_interval: 15
external_ip: null
webrtc_udp_port_min: null
webrtc_udp_port_max: null
data_storage:
  path: /Users/foo/.rings/data
  capacity: 200000000
measure_storage:
  path: /Users/foo/.rings/measure
  capacity: 200000000
"#;
    let cfg: Config = serde_saphyr::from_str(yaml).unwrap();
    assert_eq!(cfg.network_id, 1);
    assert_eq!(
        cfg.dht_virtual_nodes,
        DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
    );
    assert!(cfg.advertise_presence);
    assert!(!cfg.advertise_onion_relay);
    assert!(!cfg.advertise_onion_exit);
    assert_eq!(cfg.onion_http_proxy_addr, None);
    assert_eq!(
        cfg.onion_http_proxy_header_timeout_secs,
        crate::onion::proxy::http::default_connect_header_timeout_secs()
    );
    assert_eq!(
        cfg.onion_http_proxy_max_connections,
        crate::onion::proxy::http::default_max_connect_connections()
    );
    assert_eq!(
        cfg.onion_exit_services,
        crate::onion::default_onion_exit_services()
    );
    assert!(cfg.gateway.is_none());
    assert_eq!(cfg.remote_rpc_proxy_url, None);
    assert_eq!(cfg.api_token_path, None);
    assert!(cfg.api_allowed_origins.is_empty());
    assert!(!cfg.allow_remote_external_api);
}

#[test]
fn test_config_with_valid_remote_rpc_proxy_url_builds_processor_config() {
    let (mut config, delegatee_key_path) = config_with_delegatee_key_file();
    config.remote_rpc_proxy_url = Some("http://127.0.0.1:8080".to_string());

    let processor_config = ProcessorConfig::try_from(config);
    let _ = fs::remove_file(delegatee_key_path);

    assert!(matches!(
        processor_config
            .as_ref()
            .map(|cfg| cfg.remote_rpc_proxy.as_ref().map(|p| p.as_str())),
        Ok(Some("http://127.0.0.1:8080/"))
    ));
}

#[test]
fn test_config_with_invalid_remote_rpc_proxy_url_is_rejected() {
    let (mut config, delegatee_key_path) = config_with_delegatee_key_file();
    config.remote_rpc_proxy_url = Some("socks5://127.0.0.1:1080".to_string());

    let processor_config = ProcessorConfig::try_from(config);
    let _ = fs::remove_file(delegatee_key_path);

    assert!(matches!(processor_config, Err(Error::InvalidConfig(_))));
}

#[test]
fn test_deserialization_preserves_explicit_disabled_dht_virtual_nodes() {
    let yaml = r#"
network_id: 1
delegatee_key: delegatee_key
internal_api_port: 50000
external_api_addr: 127.0.0.1:50001
endpoint_url: http://127.0.0.1:50000
ice_servers: stun://stun.l.google.com:19302
stabilize_interval: 15
dht_virtual_nodes: 0
external_ip: null
webrtc_udp_port_min: null
webrtc_udp_port_max: null
data_storage:
  path: /Users/foo/.rings/data
  capacity: 200000000
measure_storage:
  path: /Users/foo/.rings/measure
  capacity: 200000000
"#;

    let cfg: Config = serde_saphyr::from_str(yaml).unwrap();

    assert_eq!(cfg.dht_virtual_nodes, 0);
}

const CONFIG_WITHOUT_GATEWAY_SECTION: &str = r#"
network_id: 1
delegatee_key: delegatee_key
internal_api_port: 50000
external_api_addr: 127.0.0.1:50001
endpoint_url: http://127.0.0.1:50000
ice_servers: stun://stun.l.google.com:19302
stabilize_interval: 15
external_ip: null
webrtc_udp_port_min: null
webrtc_udp_port_max: null
data_storage:
  path: /Users/foo/.rings/data
  capacity: 200000000
measure_storage:
  path: /Users/foo/.rings/measure
  capacity: 200000000
"#;

/// A hand-written section stating only what the gateway crate requires.
const GATEWAY_SECTION_WITHOUT_ENABLED: &str = r#"
gateway:
  plan:
    addresses:
    - 100.64.0.1/32
    included_routes: []
    mtu: 1280
"#;

const GATEWAY_SECTION_ENABLED: &str = r#"
gateway:
  enabled: true
  plan:
    addresses:
    - 100.64.0.1/32
    included_routes: []
    mtu: 1280
"#;

fn config_from(document: &str) -> Config {
    match serde_saphyr::from_str(document) {
        Ok(config) => config,
        Err(error) => panic!("config document must parse: {error}"),
    }
}

#[test]
fn generated_config_round_trips_with_the_gateway_disabled() {
    let root = std::env::temp_dir().join(format!("rings-config-{}", uuid::Uuid::new_v4()));
    let path = root.join("config.yaml");
    let written = Config::new("delegatee_key").write_fs(&path);
    let restored = written.and_then(Config::read_fs);
    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir(root);

    let restored = match restored {
        Ok(config) => config,
        Err(error) => panic!("generated config must round-trip: {error:?}"),
    };
    let Some(gateway) = restored.gateway.as_ref() else {
        panic!("generated config must carry a gateway section");
    };
    assert!(!gateway.enabled);
    assert_eq!(gateway.runtime.validate(), Ok(()));
    assert!(restored.enabled_gateway().is_none());
    assert_eq!(restored.origin_quota, OriginQuotaConfig::default());
}

/// The document `rings init` writes for a fresh delegatee key.
fn generated_document() -> String {
    serde_saphyr::to_string(&Config::new("delegatee_key")).expect("generated config serializes")
}

/// `rings init` states the bootstrap section explicitly, with no managed targets.
#[test]
fn generated_config_writes_an_empty_bootstrap_section() {
    let document = generated_document();
    assert!(document.contains("bootstrap:\n  peers: []\n"));
    assert_eq!(
        Config::new("delegatee_key").bootstrap,
        BootstrapConfig::default()
    );
}

/// A config written before the section existed loads with no managed targets, and an
/// explicit section round-trips its peers including an optional token.
#[test]
fn bootstrap_section_defaults_to_empty_and_round_trips_peers() {
    let base = generated_document();
    let without = base.replace("bootstrap:\n  peers: []\n", "");
    assert!(!without.contains("bootstrap:"));
    assert!(config_from(&without).bootstrap.peers.is_empty());

    let with = base.replace(
        "bootstrap:\n  peers: []\n",
        "bootstrap:\n  peers:\n  - did: 0x1\n    url: https://seed.example.org/\n  - did: \
         0x2\n    url: https://seed2.example.org/\n    api_token: secret\n",
    );
    let peers = config_from(&with).bootstrap.peers;
    assert_eq!(peers.len(), 2);
    assert_eq!(peers[0].did, "0x1");
    assert_eq!(peers[0].url, "https://seed.example.org/");
    assert_eq!(peers[0].api_token, None);
    assert_eq!(peers[1].api_token.as_deref(), Some("secret"));
}

/// The keys of a mapping, in document order; its values are ignored.
struct MappingKeys(Vec<String>);

impl<'de> Deserialize<'de> for MappingKeys {
    /// Read a mapping's keys in document order, skipping its values.
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where D: serde::Deserializer<'de> {
        /// Collects a mapping's keys in order.
        struct Keys;

        impl<'de> serde::de::Visitor<'de> for Keys {
            type Value = MappingKeys;

            /// What the visitor accepts, for the error of a non-mapping.
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a mapping")
            }

            /// Collect each key, ignoring its value.
            fn visit_map<A>(self, mut map: A) -> std::result::Result<MappingKeys, A::Error>
            where A: serde::de::MapAccess<'de> {
                let mut keys = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    map.next_value::<serde::de::IgnoredAny>()?;
                    keys.push(key);
                }
                Ok(MappingKeys(keys))
            }
        }

        deserializer.deserialize_map(Keys)
    }
}

/// A config document read for its `gateway:` section's keys alone.
#[derive(Deserialize)]
struct GatewayKeys {
    gateway: MappingKeys,
}

#[test]
fn generated_gateway_section_states_every_field() {
    let keys = serde_saphyr::from_str::<GatewayKeys>(&generated_document())
        .map(|document| document.gateway.0);

    assert_eq!(keys.ok(), Some(GATEWAY_KEYS.map(String::from).to_vec()));
}

/// Any key outside the gateway section's own is rejected, not ignored by the flattened
/// section, so a misspelt key cannot silently fall back to its default.
#[test]
fn gateway_section_rejects_unknown_keys() {
    let document = format!(
        "{CONFIG_WITHOUT_GATEWAY_SECTION}{GATEWAY_SECTION_WITHOUT_ENABLED}  \
         onion_services: https\n"
    );

    let error = match serde_saphyr::from_str::<Config>(&document) {
        Ok(_) => panic!("a misspelt gateway key must be rejected"),
        Err(error) => error.to_string(),
    };

    assert!(error.contains("unknown gateway key \"onion_services\""));
}

#[test]
fn gateway_section_without_enabled_is_inert() {
    let document = format!("{CONFIG_WITHOUT_GATEWAY_SECTION}{GATEWAY_SECTION_WITHOUT_ENABLED}");
    let config = config_from(&document);

    assert!(matches!(config.gateway, Some(ref gateway) if !gateway.enabled));
    assert!(config.enabled_gateway().is_none());
}

#[test]
fn explicitly_enabled_gateway_section_selects_a_runner() {
    let document = format!("{CONFIG_WITHOUT_GATEWAY_SECTION}{GATEWAY_SECTION_ENABLED}");
    let config = config_from(&document);

    assert!(config.enabled_gateway().is_some());
}

#[test]
fn enabling_the_generated_section_selects_a_runner() {
    let mut config = Config::new("delegatee_key");
    assert!(config.enabled_gateway().is_none());

    if let Some(gateway) = config.gateway.as_mut() {
        gateway.enabled = true;
    }

    assert!(config.enabled_gateway().is_some());
}

#[test]
fn rendered_yaml_section_completes_a_config_without_one() {
    let section = match NativeGatewayConfig::disabled_default().to_yaml_section() {
        Ok(section) => section,
        Err(error) => panic!("section must render: {error:?}"),
    };
    assert!(section.starts_with("gateway:\n"));

    let config = config_from(&format!("{CONFIG_WITHOUT_GATEWAY_SECTION}{section}"));

    assert!(matches!(config.gateway, Some(ref gateway) if !gateway.enabled));
}

#[test]
fn test_config_with_valid_webrtc_udp_range_builds_processor_config() {
    let (mut config, delegatee_key_path) = config_with_delegatee_key_file();
    config.webrtc_udp_port_min = Some(49160);
    config.webrtc_udp_port_max = Some(49200);

    let processor_config = ProcessorConfig::try_from(config);
    let _ = fs::remove_file(delegatee_key_path);

    assert!(matches!(
        processor_config.and_then(|config| config.webrtc_udp_port_range()),
        Ok(Some(range)) if range.min() == 49160 && range.max() == 49200
    ));
}

#[test]
fn test_config_with_partial_webrtc_udp_range_is_rejected() {
    let (mut config, delegatee_key_path) = config_with_delegatee_key_file();
    config.webrtc_udp_port_min = Some(49160);

    let processor_config = ProcessorConfig::try_from(config);
    let _ = fs::remove_file(delegatee_key_path);

    assert!(matches!(
        processor_config,
        Err(Error::IncompleteWebrtcUdpPortRange {
            min: Some(49160),
            max: None
        })
    ));
}

/// Raw session dumps are not reinterpreted after file lookup fails.
#[test]
fn test_raw_session_dump_is_not_treated_as_a_path_fallback() {
    let result = ProcessorConfig::try_from(Config::new(dumped_delegatee_key()));

    assert!(matches!(result, Err(Error::OpenFileError(_))));
}

/// Removed fields fail deserialization under total cutover: the signer and session-manager
/// fields, and the CONNECT proxy's service, which is always `tcp` (#834 D1′).
#[test]
fn test_removed_legacy_config_fields_are_rejected() {
    for legacy_field in ["ecdsa_key", "session_manager", "onion_http_proxy_service"] {
        let document = generated_document().replace(
            "network_id:",
            &format!("{legacy_field}: legacy\nnetwork_id:"),
        );
        let result = serde_saphyr::from_str::<Config>(&document);

        assert!(
            result.is_err(),
            "legacy field {legacy_field} must be rejected"
        );
    }
}

/// The circuit-era policy limits are refused inside `onion_exit_policy`, which the
/// top-level unknown-field rule does not reach.
#[test]
fn test_removed_onion_exit_policy_limits_are_rejected() {
    for legacy_field in ["max_circuits", "max_streams_per_circuit"] {
        let document = generated_document().replace(
            "onion_exit_policy:\n",
            &format!("onion_exit_policy:\n  {legacy_field}: 8\n"),
        );
        assert!(document.contains(legacy_field));
        let result = serde_saphyr::from_str::<Config>(&document);

        assert!(
            result.is_err(),
            "legacy policy field {legacy_field} must be rejected"
        );
    }
}

/// Write `document` to an isolated config file, read it back through [`Config::read_fs`],
/// and return the parser's message, after checking that the error is a
/// [`Error::ConfigParse`] whose message names that file and whose source is the parser's
/// error.
fn parse_failure_of(document: &str) -> String {
    let path = std::env::temp_dir().join(format!("rings-config-{}.yaml", uuid::Uuid::new_v4()));
    fs::write(&path, document).expect("write test config");
    let result = Config::read_fs(&path);
    let _ = fs::remove_file(&path);

    let Err(error) = result else {
        panic!("document must fail to parse");
    };
    let Error::ConfigParse {
        path: failed,
        source,
    } = &error
    else {
        panic!("expected a config parse error, got {error:?}");
    };
    let rendered = error.to_string();
    let detail = source.to_string();
    assert_eq!(failed, &path);
    assert!(
        rendered.contains(path.display().to_string().as_str()),
        "error must name the config file: {rendered}"
    );
    assert_eq!(
        std::error::Error::source(&error).map(ToString::to_string),
        Some(detail.clone()),
        "error must carry the parser's error as its source"
    );
    detail
}

/// The generated document with its `network_id` line replaced by `line`.
fn generated_document_with_network_id_line(line: &str) -> String {
    let network_id_line = format!("network_id: {DEFAULT_NETWORK_ID}\n");
    let document = generated_document();
    assert!(document.contains(network_id_line.as_str()));
    document.replace(network_id_line.as_str(), line)
}

/// A config without the required `network_id` names the missing key.
#[test]
fn test_read_fs_reports_missing_network_id() {
    let detail = parse_failure_of(&generated_document_with_network_id_line(""));

    assert!(
        detail.contains("missing field `network_id`"),
        "detail: {detail}"
    );
}

/// A key `Config` does not declare names that key.
#[test]
fn test_read_fs_reports_unknown_key() {
    let document = format!("{}not_a_config_key: 1\n", generated_document());

    let detail = parse_failure_of(&document);

    assert!(
        detail.contains("unknown field `not_a_config_key`"),
        "detail: {detail}"
    );
}

/// `Config::new` omits `delegator_key` when serialized, and `Config::read_fs` resolves a
/// relative `delegator_key` against the config file's parent directory (#1153).
#[test]
fn test_delegator_key_defaults_to_none_and_resolves_relative_to_config_dir() {
    let document = generated_document();
    assert!(!document.contains("delegator_key:"));
    assert_eq!(Config::new("delegatee_key").delegator_key, None);

    let root = std::env::temp_dir().join(format!("rings-config-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).expect("create temp config dir");
    let path = root.join("config.yaml");
    let mut config = Config::new("delegatee_key");
    config.delegator_key = Some("keys/ecdsa_key".to_string());
    config
        .write_fs(&path)
        .expect("write config with delegator_key");

    let restored = Config::read_fs(&path).expect("read config with delegator_key");
    let _ = fs::remove_dir_all(&root);

    assert_eq!(
        restored.delegator_key.as_deref(),
        Some(root.join("keys/ecdsa_key").to_string_lossy().as_ref())
    );
}

/// Malformed YAML, here a quoted scalar left open, reports the scanner's error and where
/// the document breaks.
#[test]
fn test_read_fs_reports_broken_yaml() {
    let document = generated_document_with_network_id_line("network_id: \"1\n");

    let detail = parse_failure_of(&document);

    assert!(detail.contains("quoted scalar"), "detail: {detail}");
    assert!(detail.contains("line 2"), "detail: {detail}");
}

/// Run `docker/cluster/rings-cluster-entrypoint.sh` against a stub `RINGS_BIN` that writes a
/// valid delegatee key on `new-delegation` and exits immediately on `run`, so the entrypoint
/// writes `config/node-$i.yaml` for each node and stops at the first readiness check.
#[cfg(unix)]
fn run_cluster_entrypoint_config_pass(
    bootstrap_peers: Option<&str>,
) -> (std::process::Output, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let root = std::env::temp_dir().join(format!("rings-cluster-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).expect("create temp cluster root");
    let dumped_key_path = root.join("dumped.delegatee_key");
    fs::write(&dumped_key_path, dumped_delegatee_key()).expect("write dumped delegatee key");
    let stub_bin = root.join("rings-stub.sh");
    let stub_script = format!(
        "#!/usr/bin/env bash\n\
         set -euo pipefail\n\
         prev=\"\"\n\
         for arg in \"$@\"; do\n\
             if [[ \"$prev\" == \"--delegatee-key\" ]]; then\n\
                 cp {} \"$arg\"\n\
                 exit 0\n\
             fi\n\
             prev=\"$arg\"\n\
         done\n\
         exit 0\n",
        dumped_key_path.display()
    );
    fs::write(&stub_bin, stub_script).expect("write stub rings bin");
    fs::set_permissions(&stub_bin, fs::Permissions::from_mode(0o755))
        .expect("chmod stub rings bin");

    let entrypoint = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docker/cluster/rings-cluster-entrypoint.sh");
    let cluster_dir = root.join("cluster");
    let mut command = Command::new("bash");
    command
        .arg(&entrypoint)
        .env("RINGS_BIN", &stub_bin)
        .env("RINGS_CLUSTER_DIR", &cluster_dir)
        .env("RINGS_NODE_COUNT", "2")
        .env("RINGS_READY_RETRIES", "1")
        .env("RINGS_READY_SLEEP_SECONDS", "0");
    if let Some(peers) = bootstrap_peers {
        command.env("RINGS_BOOTSTRAP_PEERS", peers);
    } else {
        command.env_remove("RINGS_BOOTSTRAP_PEERS");
    }
    let output = command.output().expect("execute cluster entrypoint");
    (output, root)
}

/// `docker/cluster/rings-cluster-entrypoint.sh` writes `bootstrap.peers` into each node's
/// generated YAML (`node-$i.yaml`), defaulting to `[]` and populating validated
/// `<did>=<url>` targets when `RINGS_BOOTSTRAP_PEERS` is set (#1154).
#[cfg(unix)]
#[test]
fn test_cluster_entrypoint_writes_bootstrap_peers_into_every_node_config() {
    let local_did = Did::from(1);
    let (_, default_root) = run_cluster_entrypoint_config_pass(None);
    for index in 0..2 {
        let path = default_root
            .join("cluster/config")
            .join(format!("node-{index}.yaml"));
        let config = Config::read_fs(&path).expect("generated default config parses");
        assert!(config.bootstrap.peers.is_empty());
        assert!(
            BootstrapTargets::from_config(config.bootstrap.clone(), local_did)
                .expect("empty bootstrap peers validate")
                .is_empty()
        );
        ProcessorConfig::try_from(config).expect("generated config builds ProcessorConfig");
    }
    let _ = fs::remove_dir_all(default_root);

    let first_did = "0x1111111111111111111111111111111111111111";
    let second_did = "0x2222222222222222222222222222222222222222";
    let env_value =
        format!(" {first_did} = https://node.rings.rs , {second_did}=https://seed.rings.rs:8443/ ");
    let (_, seeded_root) = run_cluster_entrypoint_config_pass(Some(env_value.as_str()));
    for index in 0..2 {
        let path = seeded_root
            .join("cluster/config")
            .join(format!("node-{index}.yaml"));
        let config = Config::read_fs(&path).expect("generated seeded config parses");
        assert_eq!(config.bootstrap.peers.len(), 2);
        assert_eq!(
            config
                .bootstrap
                .peers
                .first()
                .map(|peer| (peer.did.as_str(), peer.url.as_str())),
            Some((first_did, "https://node.rings.rs"))
        );
        assert_eq!(
            config
                .bootstrap
                .peers
                .get(1)
                .map(|peer| (peer.did.as_str(), peer.url.as_str())),
            Some((second_did, "https://seed.rings.rs:8443/"))
        );
        let targets = BootstrapTargets::from_config(config.bootstrap.clone(), local_did)
            .expect("seeded bootstrap peers validate");
        assert_eq!(targets.len(), 2);
        ProcessorConfig::try_from(config).expect("seeded config builds ProcessorConfig");
    }
    let _ = fs::remove_dir_all(seeded_root);
}

/// Malformed `RINGS_BOOTSTRAP_PEERS` values fail upfront before any node config is written.
#[cfg(unix)]
#[test]
fn test_cluster_entrypoint_rejects_malformed_bootstrap_peers() {
    for invalid in [
        "0x1111111111111111111111111111111111111111",
        "=https://node.rings.rs",
        "0x1111111111111111111111111111111111111111=",
        " , ",
    ] {
        let (output, root) = run_cluster_entrypoint_config_pass(Some(invalid));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !output.status.success(),
            "invalid RINGS_BOOTSTRAP_PEERS={invalid:?} must fail"
        );
        assert!(
            stdout.contains("ERROR: RINGS_BOOTSTRAP_PEERS"),
            "stdout must report RINGS_BOOTSTRAP_PEERS error for {invalid:?}, got: {stdout}"
        );
        assert!(
            !root.join("cluster/config/node-0.yaml").exists(),
            "entrypoint must fail before writing node configs for {invalid:?}"
        );
        let _ = fs::remove_dir_all(root);
    }
}
