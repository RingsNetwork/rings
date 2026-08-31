# rings-gateway

`rings-gateway` is the native packet-to-stream gateway used by Rings nodes. It owns the
runtime-neutral gateway and flow models. Operating-system packet devices, routing, DNS, and
cleanup are isolated under `bindings`.

The crate is intentionally unavailable to WebAssembly targets. Browser builds remain Rings
clients and do not contain the native gateway or server runtime.

## Process model

`rings run` remains a foreground Unix-style process. This crate does not install, start, stop, or
wrap systemd, launchd, the Windows Service Control Manager, or another supervisor.

On Linux and macOS, privileged network configuration is isolated in the separately launched
`gateway-config-unix` foreground helper. The node keeps one mode-0600 Unix socket connected for the
whole gateway lease. The helper creates the TUN/utun interface, installs routes, passes the packet
descriptor with `SCM_RIGHTS`, and owns cleanup. It retains a separate descriptor after transfer. A
normal teardown removes routes in reverse order; a node disconnect or malformed control stream
keeps the interface and routes alive so captured traffic blackholes rather than reverting to the
physical default route. A restarted node may resume that lease by reconnecting with the same plan.
The helper also checks kernel-reported peer credentials: the connecting UID must match
`--socket-owner`, or the helper's effective UID when no owner override is supplied.

The helper never invokes privilege elevation itself. Operators choose the narrow mechanism:

- Linux can grant only `CAP_NET_ADMIN` to the helper executable or run it in an already-privileged
  container/user namespace.
- macOS can launch the helper explicitly with root privileges. `--socket-owner UID` makes the
  mode-0600 control socket accessible to exactly one unprivileged node user.
- Windows uses the in-process Wintun and route binding and therefore requires the foreground node
  process to have the corresponding host privileges.

Build and run a Unix helper for one node lifecycle:

```bash
cargo build --release -p rings-gateway --bin gateway-config-unix

# Linux option: grant only the network-administration capability once.
sudo setcap cap_net_admin=ep target/release/gateway-config-unix
target/release/gateway-config-unix \
  --socket "$HOME/.rings/gateway-helper.sock" \
  --ledger "$HOME/.rings/gateway-routes.json" \
  --interface rings0

# macOS option: keep elevation explicit and leave the helper in the foreground.
sudo install -d -o root -g wheel -m 0755 /var/db/rings-gateway
sudo target/release/gateway-config-unix \
  --socket "/var/db/rings-gateway/helper-$(id -u).sock" \
  --socket-owner "$(id -u)" \
  --ledger "/var/db/rings-gateway/routes-$(id -u).json"
```

Both direct parents must belong to the helper's effective UID. Every canonicalized ancestor must
belong to that UID or root and must reject group/other writes. The socket itself is then assigned
mode 0600 and, when requested, chowned to the authorized node UID. This prevents a privileged
helper from following a user-controlled socket or ledger path after validation.

Then run the ordinary node in a separate terminal:

```bash
rings run --gateway --config "$HOME/.rings/config.yaml"
```

## Node configuration

The current milestone is IPv4/TCP. UDP and fragmented IPv4 fail closed; IPv6 configuration is
rejected, while host IPv6 traffic is routed into the packet interface by `::/1` and `8000::/1` and
dropped there. IPv6 therefore fails closed while forwarding support is deferred. Both DNS policies
require the operator to list every IPv4 resolver used by applications; Rings does not discover or
mutate system DNS. With `dns_policy: block`, each listed resolver gets a more-specific capture
route; captured UDP is dropped, while TCP port 53 is refused with a reset. With
`dns_policy: bypass`, each listed resolver gets a more-specific baseline-gateway route and DNS
explicitly bypasses Onion. An omitted resolver is outside the policy guarantee, so operators must
keep this declarative list aligned with host configuration.

```yaml
# Gateway mode deliberately uses relay-only ICE. Supply an authenticated TURN server.
ice_servers: turn://username:password@turn.example.net:3478

gateway:
  enabled: true
  plan:
    routing_mode: default
    addresses: ["100.64.0.1/30"]
    included_routes: ["0.0.0.0/0"]
    excluded_routes: ["127.0.0.0/8"]
    mtu: 1280
    # The default Rings ICE server is hostname-based, so this example explicitly bypasses DNS.
    dns_policy: bypass
    dns_servers: ["1.1.1.1"]
  max_flows: 1024
  flow_idle_timeout: 300
  tcp_buffer_bytes: 65536
  # Must exactly match the foreground helper's --socket path.
  unix_helper_socket: "/var/db/rings-gateway/helper-501.sock"
  # Used directly on Windows; the Unix helper owns its separate --ledger path.
  route_ledger_path: "~/.rings/gateway-routes.json"
  # Add literal IPv4 HTTP signaling/bootstrap destinations here before using them.
  underlay_bypass_targets: ["203.0.113.20"]
  underlay_refresh_secs: 2
  onion_service: tcp
  onion_hop_count: 0
  onion_allow_short_paths: false
```

For default capture, the route binding follows OpenVPN's `def1` technique: it installs
`0.0.0.0/1` and `128.0.0.0/1` instead of deleting the host's default route. Configured exclusions,
DNS destinations under `bypass`, resolved configured TURN servers, and explicit
`underlay_bypass_targets` get more-specific baseline-gateway routes. DNS destinations under
`block` instead get more-specific capture routes. Gateway activation switches native WebRTC to
relay-only ICE and requires at least one configured TURN server. Remote SDP is data, not route
authority: host, server-reflexive, and relay candidates received from a peer never add host routes.

Before `connectPeerViaHttp` sends its first request, Rings resolves the endpoint, requires every
IPv4 result to match the fixed operator-authorized set, and pins the HTTP client to exactly those
addresses. Redirects are rejected because their destination has not crossed that gate. Add each
signaling/bootstrap destination to `underlay_bypass_targets` before capture starts; runtime RPC
input cannot punch a new route around the gateway.

## Fail-closed lifetime and platform limits

On Linux and macOS, the foreground helper is part of the kill-switch boundary. It keeps the TUN or
utun descriptor and capture routes after an ungraceful node disconnect; without a data-plane
reader, captured IPv4 and IPv6 traffic blackholes. Only an authenticated explicit teardown removes
the routes. The helper must therefore remain alive. Killing the helper itself destroys the final
interface descriptor, and the kernel may remove interface-bound routes; direct routing can then
resume. The durable ledger supports later reconciliation but cannot keep a kernel interface alive.

Windows has no separate privileged helper in this milestone. While the node process is alive, a
cleanup failure retains the Wintun device and underlay-authorization gate to fail closed. If the
whole Windows node process dies, Wintun and its interface-bound routes can disappear and direct
routing can resume. Operators requiring a process-independent Windows kill switch must supply an
external firewall or supervisor policy. `routing_mode: disabled` deliberately installs no capture
or kill-switch routes on any platform.

`dns_policy: block` requires every configured TURN server host to be a literal IPv4 address. A
hostname-based TURN URL needs DNS again when a later WebRTC peer connection starts; resolving
it once before installing routes is not a durable substitute. Rings therefore rejects that
combination instead of relying on resolver cache state. Use explicit DNS bypass for named TURN
servers, or configure numeric TURN endpoints. An underlay target also cannot equal an exact `/32`
capture route; broader capture prefixes remain valid because the underlay `/32` is more specific.
The same constraint applies to HTTP seed/signaling URLs supplied at runtime: named URLs require
`dns_policy: bypass`, while `dns_policy: block` callers must use literal IPv4 endpoints. Every
resolved address must also already be present in `underlay_bypass_targets`. Resolution or
authorization failure aborts bootstrap before the first HTTP request.

Runtime limits reject more than 16,384 flows, TCP buffers larger than 1 MiB, or a declared
four-buffer-per-flow budget above 1 GiB. These checks happen before hash tables, Tokio channels, or
per-flow byte buffers are allocated. Unix helper connection and request/response operations have a
10-second default control deadline with a hard 30-second maximum, so a wedged privileged helper
cannot keep a blocking runtime alive forever. TURN resolution during gateway startup is also bounded
to 10 seconds per endpoint.

The status endpoint is exposed only on the loopback-bound internal API when the gateway is
configured; it is never mounted on the externally bound API:

```text
GET /gateway/status
```

It reports interface, lifecycle, active-flow, last-error, and Onion-exit availability without
granting route mutation authority.

## Library model API

The lifecycle, parser, flow-table, and `TcpStack` types are deliberately public as deterministic
model and embedding surfaces, even though the node integration normally uses `GatewayRuntime`.
This is an intentional API commitment rather than accidental exposure. Direct `TcpStack` callers
own the cross-model invariant that every endpoint mirrors an admitted flow and every ingested or
rejected packet is the exact validated packet represented by its `TcpSegment`; `GatewayRuntime`
enforces that invariant automatically.

## Validation and benchmarks

The portable microbenchmark drives complete IPv4/TCP packets through `GatewayRuntime`, keeps a
representative number of flows active, and validates every echoed application payload. It uses an
in-memory `PacketIo` and echo connector, so its output measures the shared TCP/bridge data path;
it is not evidence for an OS packet device or public Onion throughput. See
[`BENCHMARKS.md`](BENCHMARKS.md) for the dated reference run and evidence boundary.

```bash
RINGS_GATEWAY_BENCH_FLOWS=256 \
RINGS_GATEWAY_BENCH_BYTES_PER_FLOW=1024 \
cargo run --release -p rings-gateway --example gateway-bench
```

For process CPU and peak-memory evidence, warm the release build once and then prefix the same
command with `/usr/bin/time -l` on macOS or `/usr/bin/time -v` on Linux. Record the benchmark JSON,
user/system CPU time, and maximum resident set together with the commit and host description.

The native node owns a separate ignored integration test that uses three real WebRTC processors,
a two-hop Onion route, and a public TCP exit while retaining in-memory packet injection:

```bash
cargo test -p rings-node \
  captured_tcp_reaches_public_http_only_through_two_hop_onion_route \
  -- --ignored --nocapture
```

Linux CI additionally runs a privileged end-to-end test in a disposable network namespace. A real
kernel `TcpStream` is routed into a real TUN descriptor, reconstructed by `GatewayRuntime`, carried
over the same two-hop Onion route, and connected to the public target by the exit in the unaffected
host namespace. Namespace isolation is necessary because running the exit behind the same host
capture route would recursively capture the exit's own public connection.

```bash
sudo target/debug/deps/rings_node-... \
  processor::tests::test_gateway::real_linux_tun_tcp_reaches_public_http_through_two_hop_onion_route \
  --ignored --exact --nocapture
```

Finally, `privileged_native_tunnel_establishes_and_cleans_up` must run natively on Linux, macOS,
and Windows. While capturing `1.0.0.0/24`, it proves real TCP/HTTP reachability to the more-specific
`1.1.1.1/32` baseline-gateway bypass, confirms `1.0.0.1` reaches the real
TUN/utun/Wintun descriptor, and verifies normal teardown plus crash-ledger reconciliation. It
requires public connectivity. On Linux and macOS,
`privileged_helper_retains_and_recovers_a_disconnected_lease` additionally launches
the real `gateway-config-unix` process and proves peer-authenticated socket control, `SCM_RIGHTS`
packet-descriptor transfer, live bypass replacement, normal teardown, fail-closed disconnect, and
same-plan lease recovery.
These evidence classes are intentionally reported separately; none alone is described as a
complete VPN or a full end-to-end platform proof.
