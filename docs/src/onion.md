# Onion Routing, Onion-Op & Kleisli Pipelines

Rings separates **overlay routing** (Chord DHT forwarding with end-to-end DID authentication)
from **circuit-level metadata privacy** (`crates/node/src/onion` paired with `rings-aez` and
`rings-network-policy`). While direct relay and E2E streams hide content from intermediaries,
the onion layer hides **who is talking to which service or exit** across multi-hop, fixed-width,
guard-closed Sphinx/AEZ loops.

## Guard-closed Sphinx/AEZ loops

Every onion circuit evaluates a pipeline `P = (f₁, ā₁) ⋙ … ⋙ (fₙ, āₙ)` over a client-sealed loop
anchored at the client's pinned **entry guard** `g`:

```text
LoopOf(P):  cl → g → r_{0,2} → h_1 → r_{1,1} → r_{1,2} → ⋯ → h_n → r_{n,1} → g → cl
```

- **Segment rule (`ONION_SEGMENT_RELAYS = 2`)**: every segment between the client and a symbol hop
  `h_k` contains two relays (`g` is always the first hop leaving `cl` and the last hop returning to
  `cl`, and appears nowhere else). No single relay is adjacent to both the client and an
  application/exit symbol.
- **Single-hop world exits (`n = 1`)**: world-facing sessions (`tcp`, `https`, `udp`) run on a
  5-position loop (4 distinct non-client hops: `g, r_{0,2}, h_1, r_{1,1}` returning through `g`).
- **Composable pipelines (`n ∈ [1, 4]`, `MAX_ONION_LOOP_HOPS = 14`)**: intermediate `invoke`
  stages compose sequentially up to 4 symbol applications per loop.

### Cryptographic framing & link defenses

- **Sphinx + AEZ v5 wide-block cipher (`rings-aez`)**: every cell uses fixed-width framing
  `α (33 B ephemeral key) ‖ β (14 × 205 B = 2870 B routing block) ‖ γ (16 B MAC) ‖ carry`
  (`ONION_HEADER_BYTES = 2919` bytes). Cell size belongs solely to one of six discrete
  `OnionLoopClass` sizes (`KiB16`, `KiB64`, `KiB256`, `MiB1`, `MiB4`, `MiB12`) and wire
  compression is explicitly disabled on application/onion traffic.
- **SURB reply blocks (`OnionSurbPool`)**: world-facing sessions attach Single-Use Reply Blocks
  to forward traffic so an exit (`tcp`, `https`, `udp`) returns frames along a guard-closed path
  without learning the client's identity or IP address.
- **Pinned entry guards**: native nodes persist up to `3` entry guards in the dedicated
  `onion-entry-guards` store (`RecordAuthority::DurableCache`), rotating them on a 30-day
  lifetime (24-hour unreachable grace, evicted after 3 consecutive timeouts with a 5-minute
  cooldown) to defend against long-term guard-discovery attacks.
- **Dormant-by-default link pacing & admission**: outbound onion lanes open in dormant mode with
  zero idle cover traffic by default (`OnionIdleFloor::OFF`, `#1314`), wake automatically when real
  onion cells arrive or depart, pace constant-rate real/cover slots across active bursts and bounded
  30-second dwell windows (with configurable `OnionIdleFloor` background cover for dedicated relays),
  bind ephemeral keys to a fresh process epoch (`OnionProcessEpoch`) aligned to 5-quantum validity
  windows, deduplicate Sphinx tags in a bounded Bloom filter (`ReplayStore`, false-positive rate
  `≤ 2⁻²⁰`), and charge admission against `SlotRoster` per-connection budgets before performing ECDH work.

## Operation alphabet (`Σ`) and node roles

Every hop in a loop executes an operation from the static byte-coded signature
`Σ = {relay} ⊎ Σ_I ⊎ Σ_W` (`crates/node/src/onion/signature.rs`):

| Symbol set | Symbols | Published registry topic | Role requirement |
|---|---|---|---|
| `{relay}` | Identity forwarding hop | `online_nodes` (`capabilities.onion_relay` = process epoch) | Base capability required for any exit or invoke role (`Reg_n ≠ ∅ ⇒ relay ∈ Reg_n`) |
| `Σ_W` (world-facing sessions) | `tcp`, `https`, `udp` | `onion_exits` (`OnionExitDescriptor`) | `OnionRole::Exit` or `OnionRole::Both` |
| `Σ_I` (one-shot invokes, 120 shapes) | `invoke<In, Out, L>` | `onion_invokes` (`OnionInvokeDescriptor`) | `OnionRole::Invoke` or `OnionRole::Both` |

## World-facing exits

World-facing exits serve the closed capability set `Σ_W = {tcp, https, udp}`.
Nodes configure exit policy via `onion_exit_services` and `onion_exit_policy` in
[`config.yaml`](advanced-topic/config.yaml.md#presence-and-onion-registries) (requiring
`advertise_onion_relay: true`, `advertise_onion_exit: true`, and a non-empty `allowed_targets`
list):

1. **`tcp`** — Bidirectional reliable stream exit used by the local HTTP `CONNECT` proxy
   (`onion_http_proxy_addr`, where errors map uniformly to `502 Bad Gateway`) and the selective
   [Native TUN Gateway](native-gateway.md). TLS terminates end-to-end between client and remote
   server inside the `tcp` tunnel.
2. **`https`** — Bounded single request/response HTTPS fetcher (`8 MiB` request/response body cap)
   backing the browser workbench and Onion WebView (`crates/webview`). Exits enforce public-IP /
   public-host checks via `rings-network-policy` (blocking loopback, link-local, CGNAT, and RFC
   1918 targets) and follow `OnionExitPolicy` allow/deny rules.
3. **`udp`** — Unconnected datagram socket exit (`crates/node/src/onion/udp`) with stateful
   per-peer IP pinholes (`OnionUdpPinholeTable`), `250 ms` socket-open padding quanta, and strict
   egress filtering via `rings-network-policy`. Powers multi-hop private Onion TURN in
   [`rings-turn-service`](turn-server.md).

## Composable Kleisli `invoke` pipelines (Onion OP)

Whereas `Σ_W` opens stateful sessions to external Internet hosts, `Σ_I` executes one-shot,
fixed-width pure or service operations directly inside the loop:

- **120 typed shapes (`invoke<In, Out, L>`)**:
  - Input size `In ∈ {0, 32, 64, 256, 1024, 16384}` bytes
  - Output size `Out ∈ {32, 64, 256, 1024, 16384}` bytes
  - Constant-time release delay `L ∈ {10ms, 100ms, 500ms, 2000ms}` (`arr + L(f)`)
- **Open backend names (`Ω`)**: each `invoke` hop matches a backend string
  `ω ∈ [a-z0-9._-]⁺` (`|ω| ≤ 32`) plus up to `61` bytes of inline sealed arguments inside the
  64-byte layer payload. Sequential stages require matching boundary widths (`Out_k = In_{k+1}`).
- **Shipped example (`turn.ticket`)**: `rings-turn-service` registers backend `"turn.ticket"` at
  shape `invoke<64,1024,500ms>` so clients obtain unlinkable `SessionTicket` TURN credentials
  without revealing their DID or IP address to the TURN operator.
- **Generic invocation**: embedded callers can query `onion_invokes` / custom registries with local
  RPC `lookupRegistry` and execute single-stage `invoke` loops with `onionInvoke` (or
  `provider.onion_invoke(...)` in Browser WASM).

## Native vs. Browser capability matrix

| Capability | Native (`rings-node` / `rings run`) | Browser WASM (`rings-browser`) |
|---|---|---|
| Onion `relay` hop | Yes | Yes |
| `invoke` client (`onionInvoke`) | Yes | Yes |
| `invoke` backend host | Yes (`OnionPipelineHandle`) | No |
| `https` client (`onion_https_proxy`) | Yes | Yes |
| `https` exit (`OnionExitService::Https`) | Yes (full exit policy) | Yes (via `new_https_exit_instance`, governed by `rings-network-policy` plus browser CORS / MV3 host permissions) |
| `tcp` client & exit | Yes | No |
| `udp` client & exit | Yes | No |
| Local HTTP `CONNECT` proxy & TUN gateway | Yes | No |

## Related chapters

- [TURN Server & Onion TURN](turn-server.md) — `turn.ticket` over `invoke` and `multi_hop_onion_udp`
- [Native Gateway](native-gateway.md) — routing IPv4/TCP OS traffic into `onion::tcp`
- [Browser Node and Extension](browser-node.md) — Onion Proxy workbench and sandboxed Onion WebView
- [config.yaml](advanced-topic/config.yaml.md#presence-and-onion-registries) — relay, exit, and proxy configuration
