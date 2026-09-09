# config.yaml

`rings init` writes the node configuration to `~/.rings/config.yaml` (or the path given to
`--location`) with every section stated explicitly, so the generated file is the one place an
operator edits. This is the file it writes, with `$HOME` expanded to `/home/operator`:

```yaml
network_id: 1
session_sk: /home/operator/.rings/session_sk
internal_api_port: 50000
external_api_addr: 127.0.0.1:50001
endpoint_url: http://127.0.0.1:50000
allow_remote_external_api: false
ice_servers: stun://stun.l.google.com:19302
stabilize_interval: 15
online_node_heartbeat_interval_secs: 30
online_node_ttl_secs: 90
online_node_type: Native
advertise_presence: true
advertise_onion_relay: false
advertise_onion_exit: false
onion_exit_heartbeat_interval_secs: 30
onion_exit_ttl_secs: 90
onion_exit_services:
- name: tcp
  transport: Tcp
- name: https
  transport: Tcp
onion_exit_policy:
  allowed_targets: []
  denied_targets: []
  max_circuits: 0
  max_streams_per_circuit: 0
  max_bytes_per_minute: 0
onion_http_proxy_service: tcp
onion_http_proxy_hop_count: 0
onion_http_proxy_allow_short_paths: false
onion_http_proxy_header_timeout_secs: 10
onion_http_proxy_max_connections: 1024
gateway:
  enabled: false
  plan:
    addresses:
    - 100.64.0.1/32
    included_routes: []
    mtu: 1280
  max_flows: 1024
  flow_idle_timeout: 300
  tcp_buffer_bytes: 65536
  interface_name: null
  route_ledger_path: /home/operator/.rings/gateway-routes.json
  unix_helper_socket: /home/operator/.rings/gateway-helper.sock
  wintun_dll_path: null
  status_refresh_secs: 2
  onion_service: tcp
  onion_hop_count: 0
  onion_allow_short_paths: false
dht_virtual_nodes: 160
data_storage:
  path: /home/operator/.rings/data
  capacity: 200000000
measure_storage:
  path: /home/operator/.rings/measure
  capacity: 200000000
```

Every field has the meaning below. Fields absent from a file take the value shown above, except
where noted.

## Identity and network

* `network_id`: the Rings overlay this node joins. Signatures are bound to it, so nodes on
  different overlays do not verify each other's messages.
* `session_sk`: path of the session secret key file that `rings init` writes next to the config.
  The session key is derived from your ECDSA key and signs every message on your behalf; keep the
  file private. Passing a raw key string here instead of a path is deprecated.
* `ice_servers`: STUN or TURN servers used to establish WebRTC connections, separated by `;`.
  STUN is ordinary discovery; TURN is optional and never a gateway prerequisite.

  ```yaml
  ice_servers: turn://user:pass@turn.example.org:3478;stun://stun.l.google.com:19302
  ```

* `stabilize_interval`: seconds between Chord stabilization rounds.
* `dht_virtual_nodes`: virtual DHT positions this node owns for storage placement; `0` disables
  virtual positions.
* `external_ip`, `webrtc_udp_port_min`, `webrtc_udp_port_max`: optional reachability hints, an
  externally visible address and a UDP port range for ICE. The two port bounds must be given
  together.

## Control API

* `internal_api_port`: loopback JSON-RPC listener for the `rings` CLI and local tooling; every
  route requires the Bearer token.
* `external_api_addr`: JSON-RPC listener peers dial for the HTTP handshake. `nodeDid` and
  `answerOffer` are public; status and registry reads require the token.
* `endpoint_url`: the internal endpoint the CLI connects to.
* `api_token_path`: optional path of the Bearer token file; by default `api-token` next to the
  config file. Relative paths are resolved next to the config file.
* `api_allowed_origins`: exact browser origins permitted to call the authenticated API; empty
  denies every browser origin.
* `allow_remote_external_api`: required to bind `external_api_addr` to a non-loopback address.

## Presence and onion registries

* `online_node_heartbeat_interval_secs`, `online_node_ttl_secs`, `online_node_type`,
  `advertise_presence`: how this node publishes its online-node descriptor.
* `advertise_onion_relay`: advertise onion relay capability.
* `advertise_onion_exit`, `onion_exit_heartbeat_interval_secs`, `onion_exit_ttl_secs`,
  `onion_exit_services`, `onion_exit_policy`: whether and how this node serves as an onion exit.
  An empty `allowed_targets` list is a closed policy that admits no target, so advertising an
  exit requires at least one allowed target; deny entries override allows; a `0` limit is
  unspecified.

## HTTP CONNECT proxy

* `onion_http_proxy_addr`: optional local HTTP CONNECT listener that routes client TCP streams
  through onion exits; absent means no proxy.
* `onion_http_proxy_service`, `onion_http_proxy_hop_count`,
  `onion_http_proxy_allow_short_paths`, `onion_http_proxy_header_timeout_secs`,
  `onion_http_proxy_max_connections`: exit service, route length, and limits of that proxy.

## Gateway

The `gateway` section configures the native TUN gateway described in
[Native Gateway](../native-gateway.md). Presence of the section is not consent to start a TUN
device:

```text
gateway starts ⟺ section present ∧ (enabled = true ∨ rings run --gateway)
```

* `enabled`: whether plain `rings run` starts the gateway. Absent means `false`.
* `plan.addresses`: IPv4 `/32` host addresses of the virtual interface. The generated
  `100.64.0.1/32` is drawn from the RFC 6598 shared address space, so it collides with neither
  LAN nor VPN ranges.
* `plan.included_routes`: the only destination prefixes routed into the gateway. Empty captures
  nothing; a default route or a set covering all of IPv4 is rejected.
* `plan.mtu`: interface MTU. The generated `1280` is the IPv6 minimum link MTU, which every
  underlay carries.
* `max_flows`, `flow_idle_timeout`, `tcp_buffer_bytes`: concurrent TCP flow limit, per-flow idle
  timeout in seconds, and per-flow buffer size.
* `interface_name`, `wintun_dll_path`: Windows interface name and Wintun DLL path; `null` selects
  the defaults. On Unix the foreground helper's `--interface` is authoritative.
* `route_ledger_path`: durable route journal, used directly on Windows; on Unix the helper's
  `--ledger` is authoritative.
* `unix_helper_socket`: control socket of the `gateway-config-unix` foreground helper on Linux
  and macOS; must match the helper's `--socket`.
* `status_refresh_secs`: refresh interval of onion-exit availability in `/gateway/status`.
* `onion_service`, `onion_hop_count`, `onion_allow_short_paths`: exit service and route length
  used for captured flows.

## Storage

* `data_storage`: path and capacity in bytes of the DHT data store.
* `measure_storage`: path and capacity in bytes of the peer measurement store.

Files written before a field existed still load: an omitted field takes its default, and an
omitted `gateway` section loads as no gateway at all.
