# rings-gateway

`rings-gateway` is the native packet-to-stream gateway used by Rings nodes. It forwards
explicitly selected IPv4/TCP flows through Rings Onion circuits. The packet mechanism does not
imply one routing policy: the Rings gateway contract is selective routing and has no full-tunnel
or kill-switch mode.

The crate is intentionally unavailable to WebAssembly targets. Browser builds remain Rings
clients and do not contain the native gateway or server runtime.

## Traffic-selection contract

The operator owns traffic selection. Let `C` be the normalized set in `included_routes`:

```text
installed destination capture routes = C
```

The platform binding deduplicates and normalizes `C`, but does not add routes derived from a
default route, IPv6, DNS, TURN, ICE, SDP, bootstrap endpoints, or observed peers. A host-wide
`0.0.0.0/0` capture, or any set of prefixes whose union covers the entire IPv4 address space, is
rejected. Interface addresses must be IPv4 `/32` host addresses so setup cannot create a connected
capture prefix. An empty set is valid when external routing owns packet selection instead.

This follows the same mechanism-versus-policy separation as OpenVPN. OpenVPN's [`--route`
option](https://openvpn.net/community-docs/community-articles/openvpn-2-7-manual.html) selects
individual prefixes, while `--redirect-gateway def1` explicitly requests full-tunnel routing and
implements it with two `/1` routes. That is a valid OpenVPN policy, but Rings intentionally has no
`redirect-gateway` equivalent. Capturing every host destination would also capture the ordinary
underlay traffic carrying existing Rings neighbor links; avoiding recursion would require dynamic
bypass state or guaranteed relay. Rings requires neither. A `/0` or equivalent covering prefix set
is therefore rejected as an intent stop-line outside the selective gateway contract. Near-cover
sets remain valid, including the entire IPv4 space minus one `/32`; this rejection is not a proof
against recursion. If the operator explicitly selects an underlay destination, it enters the same
fail-closed path as any other selected destination and Rings does not create a direct bypass.

The gateway's fail-closed guarantee applies only after traffic enters it. A selected flow either
opens its immutable target through a valid Onion route and compatible TCP exit, or fails without
direct fallback. Traffic outside `C` continues to use ordinary host routing. Rings bootstrap, SDP
messages, and ICE also keep their ordinary routing behavior. STUN remains an ordinary discovery
facility. TURN may be configured, but gateway and server capability must never depend on it or
force relay-only ICE.

Traffic can enter the shared Onion egress through:

- an exact host route or destination prefix installed by the packet binding;
- an external router or policy-routing rule owned by the operator; or
- an explicit application proxy such as HTTP CONNECT or SOCKS5.

The existing native HTTP CONNECT proxy and packet ingress share Onion target validation, route
construction, TCP stream opening, bidirectional transfer, half-close, timeout, and accounting.
A future SOCKS5 frontend should add only ingress parsing and authentication policy.

Pointing several DNS names at one gateway address does not preserve the original generic TCP
target. Optional DNS integration therefore needs a dedicated synthetic prefix and an explicit
`synthetic IP -> original target` mapping. Rings does not discover or mutate system DNS.

## Process model

`rings run` remains a foreground Unix-style process. This crate does not install, start, stop, or
wrap systemd, launchd, the Windows Service Control Manager, or another supervisor.

On Linux and macOS, privileged interface and route effects are isolated in the separately launched
`gateway-config-unix` foreground helper. The node keeps one mode-0600 Unix socket connected for the
gateway lease. The helper creates the TUN/utun interface, installs only `included_routes`, passes
the packet descriptor with `SCM_RIGHTS`, and owns cleanup. A normal teardown removes owned routes
in reverse order. A node disconnect leaves the interface and selected routes alive so those
selected destinations remain fail-closed; a restarted node may resume the lease with the same
plan. Unrelated host traffic remains unaffected.

The helper never invokes privilege elevation. Operators choose the narrow mechanism:

- Linux can grant only `CAP_NET_ADMIN` to the helper or run it in an already-privileged namespace.
- macOS can launch the helper explicitly as root and use `--socket-owner UID` for one node user.
- Windows uses the in-process Wintun and route binding, so the foreground node needs the relevant
  host privileges.

Build and run a Unix helper for one node lifecycle:

```bash
cargo build --release -p rings-gateway --bin gateway-config-unix

# Linux
sudo setcap cap_net_admin=ep target/release/gateway-config-unix
target/release/gateway-config-unix \
  --socket "$HOME/.rings/gateway-helper.sock" \
  --ledger "$HOME/.rings/gateway-routes.json" \
  --interface rings0

# macOS
sudo install -d -o root -g wheel -m 0755 /var/db/rings-gateway
sudo target/release/gateway-config-unix \
  --socket "/var/db/rings-gateway/helper-$(id -u).sock" \
  --socket-owner "$(id -u)" \
  --ledger "/var/db/rings-gateway/routes-$(id -u).json"
```

Both direct parents must belong to the helper's effective UID. Every canonicalized ancestor must
belong to that UID or root and reject group/other writes. The socket is mode 0600 and may be
assigned to the authorized node UID. Startup probes an existing socket and refuses to unlink it
while a helper is listening; only an unreachable stale socket node is removed.

Then run the node separately:

```bash
rings run --gateway --config "$HOME/.rings/config.yaml"
```

## Node configuration

The current packet milestone is IPv4/TCP. Only the listed destination prefixes enter the packet
gateway. UDP or fragmented IPv4 packets that enter a listed prefix are dropped; IPv6 routes are
rejected at configuration time. Unlisted IPv4, all IPv6, and ordinary DNS traffic remain outside
gateway policy.

```yaml
# STUN remains ordinary discovery. TURN is optional and never a gateway prerequisite.
ice_servers: stun://stun.l.google.com:19302

gateway:
  enabled: true
  plan:
    # Host prefixes are required so interface setup cannot create a connected capture subnet.
    addresses: ["100.64.0.1/32"]
    # Only these destinations are routed to TUN/utun/Wintun.
    included_routes: ["1.1.1.1/32"]
    mtu: 1280
  max_flows: 1024
  flow_idle_timeout: 300
  tcp_buffer_bytes: 65536
  # Must match the foreground helper's --socket path on Linux/macOS.
  unix_helper_socket: "/var/db/rings-gateway/helper-501.sock"
  # Used directly on Windows; the Unix helper owns its separate --ledger path.
  route_ledger_path: "~/.rings/gateway-routes.json"
  status_refresh_secs: 2
  onion_service: tcp
  onion_hop_count: 0
  onion_allow_short_paths: false
```

For externally steered packet operation, `included_routes` may be empty. The packet interface
still exists, but Rings installs no destination route:

```yaml
gateway:
  plan:
    addresses: ["100.64.0.1/32"]
    included_routes: []
    mtu: 1280
```

For application-proxy-only operation, leave `gateway` disabled and configure the existing native
HTTP CONNECT listener. That path creates no packet interface. SOCKS5 remains a future frontend to
the same Onion egress.

Runtime input never expands the capture set. If an operator deliberately selects a destination
that Rings itself uses for bootstrap or ICE, the configured route has the same effect as any other
operator route; Rings does not silently punch a bypass around it.

At establishment, the native binding reads one route-table snapshot and rejects every exact or
more-specific existing route inside a capture prefix because longest-prefix routing would let that
sub-range bypass the gateway. Rings deliberately has no route-change subscription. A route added
after that snapshot, including while establishment finishes, can therefore shadow a capture prefix
and send that sub-range through the host's new route; operators must prevent or monitor such
route-table mutation. The fail-closed guarantee begins only after a packet actually enters the
gateway.

Removed route-authority fields such as `routing_mode`, `excluded_routes`, `dns_policy`, and
`dns_servers` are rejected inside `plan` instead of being silently ignored. The gateway never
interprets old full-capture configuration as a new selective-capture plan.

## Cleanup and status

The durable route ledger contains only explicitly selected capture routes. Its persistent `.lock`
sidecar is held exclusively for the complete native-controller lifetime, including startup
reconciliation, so a second helper or Windows node using the same ledger fails before it can touch
the first instance's live routes. The sidecar file may remain after shutdown; the operating system
releases its advisory lock when the process exits. Startup reconciliation removes only stale owned
routes. On macOS it first requires the current exact route to carry the interface identity recorded
in the ledger; an absent route clears the intent, while a foreign or ambiguous owner is preserved
and reported rather than deleted. A failed teardown returns its linear lease so cleanup remains
retryable. On Unix, the helper retains its packet descriptor after transfer. On Windows, the node
retains the Wintun device after a cleanup failure. These behaviors keep affected selected
destinations fail-closed without claiming control over unrelated traffic.

Runtime limits reject more than 16,384 flows, TCP buffers larger than 1 MiB, or a declared
four-buffer-per-flow budget above 1 GiB. The Unix client bounds connection and response operations
to 10 seconds by default with a hard 30-second maximum.

The loopback-only internal API exposes gateway status when configured:

```text
GET /gateway/status
```

It reports interface, lifecycle, normalized capture routes, active-flow count, last error, and
Onion-exit availability without granting route mutation authority.

## Library model API

The lifecycle, parser, flow-table, and `TcpStack` types are public deterministic model and embedding
surfaces. Direct `TcpStack` callers own the invariant that every endpoint mirrors an admitted flow
and every packet is the validated packet represented by its `TcpSegment`; `GatewayRuntime`
enforces that invariant automatically.

## Validation and benchmarks

The portable microbenchmark drives complete IPv4/TCP packets through `GatewayRuntime` with an
in-memory packet device and echo connector. It measures the shared TCP/bridge path, not OS packet
IO or public Onion throughput. See [`BENCHMARKS.md`](BENCHMARKS.md).

```bash
RINGS_GATEWAY_BENCH_FLOWS=256 \
RINGS_GATEWAY_BENCH_BYTES_PER_FLOW=1024 \
cargo run --release -p rings-gateway --example gateway-bench
```

The native node integration test uses real WebRTC processors, a two-hop Onion route, and a public
TCP exit with in-memory packet injection:

```bash
cargo test -p rings-node \
  captured_tcp_reaches_public_http_only_through_two_hop_onion_route \
  -- --ignored --nocapture
```

Privileged platform tests install `1.0.0.0/24` as the sole capture route, prove that `1.0.0.1`
arrives at the packet descriptor, prove that unselected `1.1.1.1` remains reachable through the
ordinary route, and verify teardown plus stale-ledger recovery. Unix additionally tests the real
helper process, peer authentication, `SCM_RIGHTS` transfer, disconnected-lease retention, and
same-plan recovery. The ledger assertion rejects hidden IPv4 or IPv6 catch-all routes.

These evidence classes are reported separately; none is described as a complete VPN.
